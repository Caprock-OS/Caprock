#![no_std]
#![forbid(unsafe_code)]
//! Minimaler Parser für einen Flattened Device Tree (FDT/DTB).
//!
//! Platform queries (cells-aware `reg`, `compatible`, GIC, timer, `/chosen`, framebuffers,
//! memory and reserved memory, PCIe ECAM, virtio-mmio) are in the `platform queries` section
//! at the end of this file; they are built on the generic [`Dtb::walk`] node walker.
//!
//! Liest die Plattformbeschreibung (hier: die RAM-Region aus dem `/memory`-
//! Knoten) statt sie fest zu verdrahten. Reines, **sicheres** Parsen über einen
//! `&[u8]`-Slice (alle Zugriffe bounds-checked, **kein `unsafe`**).
//!
//! In einem echten System übergibt der Bootloader den DTB-Zeiger (aarch64: `x0`).
//! Da QEMU für ein rohes `-kernel`-ELF (ohne Linux-Image-Header) keinen DTB
//! ablegt, betten wir den von QEMU erzeugten DTB ein und parsen ihn — der Parser
//! arbeitet auf einem echten Device Tree.

const MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

/// Geparster Device Tree.
pub struct Dtb<'a> {
    data: &'a [u8],
    off_struct: usize,
    off_strings: usize,
}

fn be32(data: &[u8], off: usize) -> Option<u32> {
    let b = data.get(off..off + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn be64(data: &[u8], off: usize) -> Option<u64> {
    let hi = be32(data, off)? as u64;
    let lo = be32(data, off + 4)? as u64;
    Some((hi << 32) | lo)
}

/// Null-terminierten String ab `off` zurückgeben (ohne das `\0`).
fn cstr(data: &[u8], off: usize) -> &[u8] {
    let rest = match data.get(off..) {
        Some(r) => r,
        None => return &[],
    };
    let end = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
    &rest[..end]
}

const fn align4(n: usize) -> usize {
    (n + 3) & !3
}

impl<'a> Dtb<'a> {
    /// Einen DTB-Slice parsen (prüft Magic + Header).
    pub fn parse(data: &'a [u8]) -> Option<Dtb<'a>> {
        if be32(data, 0)? != MAGIC {
            return None;
        }
        let off_struct = be32(data, 8)? as usize;
        let off_strings = be32(data, 12)? as usize;
        Some(Dtb {
            data,
            off_struct,
            off_strings,
        })
    }

    /// **Anzahl der CPUs** aus den `cpu@…`-Knoten unterhalb von `/cpus` (ext-30).
    ///
    /// Die Kernzahl wird damit von der Plattform *gelesen* statt fest verdrahtet — Grundlage
    /// für die zur Boot-Zeit dimensionierten Scheduler-Tabellen. Gezählt werden Knoten, deren
    /// Name mit `cpu@` beginnt **und** die direkt unter `/cpus` liegen (Tiefe 2); `cpu-map`-
    /// Untereinträge (Cluster-Topologie) heißen anders und werden nicht mitgezählt.
    /// `None`, wenn der Baum unlesbar ist; `Some(0)`, wenn es keine `cpu@`-Knoten gibt.
    pub fn cpu_count(&self) -> Option<usize> {
        let mut pos = self.off_struct;
        let mut depth = 0usize;
        let mut in_cpus_at = usize::MAX; // Tiefe des `/cpus`-Knotens
        let mut n = 0usize;
        loop {
            let tok = be32(self.data, pos)?;
            pos += 4;
            match tok {
                FDT_BEGIN_NODE => {
                    let name = cstr(self.data, pos);
                    pos += align4(name.len() + 1);
                    depth += 1;
                    if depth == 2 && name == b"cpus" {
                        in_cpus_at = depth;
                    } else if in_cpus_at != usize::MAX
                        && depth == in_cpus_at + 1
                        && name.starts_with(b"cpu@")
                    {
                        n += 1;
                    }
                }
                FDT_END_NODE => {
                    if depth == in_cpus_at {
                        in_cpus_at = usize::MAX; // `/cpus` verlassen
                    }
                    depth = depth.saturating_sub(1);
                }
                FDT_PROP => {
                    let len = be32(self.data, pos)? as usize;
                    pos = pos + 8 + align4(len);
                }
                FDT_NOP => {}
                FDT_END => return Some(n),
                _ => return None,
            }
        }
    }

    /// **NUMA affinities from the device tree** (Z8/N0) — the aarch64 counterpart to ACPI SRAT.
    ///
    /// Calls `mem(base, size, node)` for every `memory@…` node that carries a `numa-node-id`, and
    /// `cpu(index, node)` for every `cpu@…` node under `/cpus` that does. Returns the number of
    /// affinities reported, or `None` if the tree is unreadable.
    ///
    /// **Callbacks rather than a returned structure, on purpose.** This crate is dependency-free
    /// and stays that way; the classification (what an unaffiliated range means, what happens when
    /// storage runs out) belongs in one place for both architectures, and that place is
    /// `caprock_hal::numa`. Two crates deciding independently what a node is would be two designs.
    ///
    /// The CPU index is the **ordinal** of the `cpu@` node under `/cpus`, which is what
    /// `MPIDR_EL1.Aff0` reports on QEMU `virt` — the same identity `hal::cpu::core_id` uses. On a
    /// machine where those differ this needs the `reg` property instead, and it would be wrong
    /// silently; that is why the report line prints how many CPU affinities were matched.
    pub fn numa(
        &self,
        mut mem: impl FnMut(u64, u64, u32),
        mut cpu: impl FnMut(u32, u32),
    ) -> Option<usize> {
        let mut pos = self.off_struct;
        let mut depth = 0usize;
        let mut in_cpus_at = usize::MAX;
        // Zustand des GERADE offenen Knotens: `reg` und `numa-node-id` koennen in beliebiger
        // Reihenfolge kommen, also erst am `FDT_END_NODE` auswerten.
        let mut is_mem = false;
        let mut is_cpu = false;
        let mut cpu_ord = 0u32;
        let mut this_cpu_ord = 0u32;
        let mut reg: Option<(u64, u64)> = None;
        let mut node_id: Option<u32> = None;
        let mut n = 0usize;
        loop {
            let tok = be32(self.data, pos)?;
            pos += 4;
            match tok {
                FDT_BEGIN_NODE => {
                    let name = cstr(self.data, pos);
                    pos += align4(name.len() + 1);
                    depth += 1;
                    if depth == 2 && name == b"cpus" {
                        in_cpus_at = depth;
                    }
                    is_mem = name.starts_with(b"memory");
                    is_cpu = in_cpus_at != usize::MAX
                        && depth == in_cpus_at + 1
                        && name.starts_with(b"cpu@");
                    if is_cpu {
                        this_cpu_ord = cpu_ord;
                        cpu_ord += 1;
                    }
                    reg = None;
                    node_id = None;
                }
                FDT_END_NODE => {
                    if let Some(nd) = node_id {
                        if is_mem {
                            if let Some((b, l)) = reg {
                                mem(b, l, nd);
                                n += 1;
                            }
                        } else if is_cpu {
                            cpu(this_cpu_ord, nd);
                            n += 1;
                        }
                    }
                    if depth == in_cpus_at {
                        in_cpus_at = usize::MAX;
                    }
                    depth = depth.saturating_sub(1);
                    is_mem = false;
                    is_cpu = false;
                    reg = None;
                    node_id = None;
                }
                FDT_PROP => {
                    let len = be32(self.data, pos)? as usize;
                    let nameoff = be32(self.data, pos + 4)? as usize;
                    let val = pos + 8;
                    pos = val + align4(len);
                    let pname = cstr(self.data, self.off_strings + nameoff);
                    if pname == b"numa-node-id" && len >= 4 {
                        node_id = Some(be32(self.data, val)?);
                    } else if pname == b"reg" && is_mem && len >= 16 {
                        reg = Some((be64(self.data, val)?, be64(self.data, val + 8)?));
                    }
                }
                FDT_NOP => {}
                FDT_END => return Some(n),
                _ => return None,
            }
        }
    }

    /// The first non-empty RAM region: `(base, size)`.
    ///
    /// Thin wrapper over [`Dtb::for_each_memory`], which honours `#address-cells` and
    /// `#size-cells` (the old version assumed 2/2). On QEMU `virt` the result is unchanged.
    pub fn memory(&self) -> Option<(u64, u64)> {
        let mut first = None;
        self.for_each_memory(|base, size| {
            if first.is_none() && size != 0 {
                first = Some((base, size));
            }
        })?;
        first
    }
}

// ======================================================================================
// Platform queries
//
// Everything below is English by the repository language rule. It is built on one generic
// walker, `Dtb::walk`, which hands every node to a callback together with its properties,
// the cell sizes of its parent, and whether its `reg` is expressed in CPU physical addresses.
// All reads are bounds-checked; malformed input yields `None`, never a panic.
// ======================================================================================

/// Maximum node nesting depth the walker accepts (deeper trees are rejected with `None`).
pub const MAX_DEPTH: usize = 16;

/// Cell sizes when a node does not carry `#address-cells` / `#size-cells` (devicetree spec 2.3.5).
const DEFAULT_ADDR_CELLS: u32 = 2;
const DEFAULT_SIZE_CELLS: u32 = 1;

/// One node of the tree, as seen by [`Dtb::walk`]. Cheap to copy; borrows the blob.
#[derive(Clone, Copy)]
pub struct Node<'a> {
    data: &'a [u8],
    off_strings: usize,
    name: &'a [u8],
    depth: usize,
    /// `names[0]` is the root's (empty) name, `names[depth - 1]` is this node's name.
    names: [&'a [u8]; MAX_DEPTH],
    parent_addr_cells: u32,
    parent_size_cells: u32,
    props: usize,
    props_end: usize,
    cpu_addressable: bool,
}

/// Iterator over `(name, value)` of a node's properties.
pub struct Props<'a> {
    data: &'a [u8],
    off_strings: usize,
    pos: usize,
    end: usize,
}

impl<'a> Iterator for Props<'a> {
    type Item = (&'a [u8], &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        while self.pos < self.end {
            let tok = be32(self.data, self.pos)?;
            if tok == FDT_NOP {
                self.pos += 4;
                continue;
            }
            if tok != FDT_PROP {
                return None;
            }
            let len = be32(self.data, self.pos + 4)? as usize;
            let nameoff = be32(self.data, self.pos + 8)? as usize;
            let val = self.pos.checked_add(12)?;
            let val_end = val.checked_add(len)?;
            let value = self.data.get(val..val_end)?;
            self.pos = val.checked_add(align4(len))?;
            let name = cstr(self.data, self.off_strings.checked_add(nameoff)?);
            return Some((name, value));
        }
        None
    }
}

/// Read `cells` big-endian 32-bit cells as one value. Only the low 64 bits are kept; `None`
/// if the slice is too short. Zero cells read as 0.
fn read_cells(v: &[u8], cells: u32) -> Option<u64> {
    let mut acc = 0u64;
    for i in 0..cells as usize {
        let c = be32(v, i.checked_mul(4)?)? as u64;
        acc = (acc << 32) | c;
    }
    Some(acc)
}

impl<'a> Node<'a> {
    /// Full node name including the unit address, e.g. `pl011@9000000` (root: empty).
    pub fn name(&self) -> &'a [u8] {
        self.name
    }

    /// Node name without the `@unit-address` part.
    pub fn base_name(&self) -> &'a [u8] {
        match self.name.iter().position(|&c| c == b'@') {
            Some(i) => &self.name[..i],
            None => self.name,
        }
    }

    /// Nesting depth: the root is 1, its children 2, and so on.
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// `(#address-cells, #size-cells)` that apply to this node's `reg`.
    pub fn parent_cells(&self) -> (u32, u32) {
        (self.parent_addr_cells, self.parent_size_cells)
    }

    /// `true` if every enclosing `ranges` is the identity map (or absent at the root), i.e. the
    /// addresses in this node's `reg` are CPU physical addresses. `false` for example below the
    /// QEMU `platform-bus`, whose `ranges` shifts addresses. Translation is NOT applied by this
    /// crate; callers decide what to do with a node that is not CPU-addressable.
    pub fn cpu_addressable(&self) -> bool {
        self.cpu_addressable
    }

    /// Iterate this node's properties.
    pub fn props(&self) -> Props<'a> {
        Props {
            data: self.data,
            off_strings: self.off_strings,
            pos: self.props,
            end: self.props_end,
        }
    }

    /// Raw value of property `name`.
    pub fn prop(&self, name: &str) -> Option<&'a [u8]> {
        self.props().find(|(n, _)| *n == name.as_bytes()).map(|(_, v)| v)
    }

    /// `true` if the (valueless or valued) property exists.
    pub fn has_prop(&self, name: &str) -> bool {
        self.prop(name).is_some()
    }

    /// A single-cell property.
    pub fn prop_u32(&self, name: &str) -> Option<u32> {
        let v = self.prop(name)?;
        if v.len() != 4 {
            return None;
        }
        be32(v, 0)
    }

    /// A property holding one NUL-terminated string (without the NUL).
    pub fn prop_str(&self, name: &str) -> Option<&'a str> {
        let v = self.prop(name)?;
        let end = v.iter().position(|&c| c == 0)?;
        core::str::from_utf8(&v[..end]).ok()
    }

    /// Iterate the strings of the `compatible` property (most specific first).
    pub fn compatibles(&self) -> impl Iterator<Item = &'a [u8]> {
        self.prop("compatible")
            .unwrap_or(&[])
            .split(|&c| c == 0)
            .filter(|s| !s.is_empty())
    }

    /// `true` if `compatible` lists `needle`.
    pub fn is_compatible(&self, needle: &str) -> bool {
        self.compatibles().any(|c| c == needle.as_bytes())
    }

    /// `true` if the node is enabled: no `status`, or `"okay"` / `"ok"`.
    pub fn is_enabled(&self) -> bool {
        match self.prop("status") {
            None => true,
            Some(v) => v == b"okay\0" || v == b"ok\0",
        }
    }

    /// Number of `(address, size)` entries in `reg`, using the parent's cell sizes.
    pub fn reg_count(&self) -> usize {
        let stride = (self.parent_addr_cells + self.parent_size_cells) as usize * 4;
        match self.prop("reg") {
            Some(v) if stride != 0 => v.len() / stride,
            _ => 0,
        }
    }

    /// The `i`-th `reg` entry as `(address, size)`. `None` if absent, truncated, or if the
    /// address or size is wider than two cells. With `#size-cells = 0` the size is 0.
    pub fn reg(&self, i: usize) -> Option<(u64, u64)> {
        let (ac, sc) = (self.parent_addr_cells, self.parent_size_cells);
        if ac == 0 || ac > 2 || sc > 2 {
            return None;
        }
        let v = self.prop("reg")?;
        let stride = (ac + sc) as usize * 4;
        let start = i.checked_mul(stride)?;
        let entry = v.get(start..start.checked_add(stride)?)?;
        let addr = read_cells(entry, ac)?;
        let size = read_cells(entry.get(ac as usize * 4..)?, sc)?;
        Some((addr, size))
    }

    /// `true` if the node's absolute path equals `path` (`/soc@0/serial@894000`). A path
    /// component without `@` also matches a node with a unit address (`/soc/serial`).
    pub fn path_is(&self, path: &str) -> bool {
        let p = path.as_bytes();
        if p.first() != Some(&b'/') {
            return false;
        }
        let comps = p[1..].split(|&c| c == b'/').filter(|c| !c.is_empty());
        let mut n = 1; // names[0] is the root
        for comp in comps {
            if n >= self.depth {
                return false;
            }
            let nm = self.names[n];
            let ok = nm == comp
                || (!comp.contains(&b'@')
                    && nm.split(|&c| c == b'@').next() == Some(comp));
            if !ok {
                return false;
            }
            n += 1;
        }
        n == self.depth
    }
}

/// `ranges` of `node` (child cells = its own `#address-cells`, parent cells = `parent_ac`) is the
/// identity map. An empty `ranges;` is the identity by definition; an absent one maps nothing.
fn ranges_identity(ranges: Option<&[u8]>, child_ac: u32, parent_ac: u32, size_cells: u32) -> bool {
    let v = match ranges {
        None => return false,
        Some(v) => v,
    };
    if v.is_empty() {
        return true;
    }
    let tuple = (child_ac + parent_ac + size_cells) as usize * 4;
    if tuple == 0 || v.len() % tuple != 0 || child_ac > 3 || parent_ac > 2 {
        return false;
    }
    let mut off = 0;
    while off < v.len() {
        // Compare the low two cells of the child address with the parent address.
        let c = v.get(off..off + child_ac as usize * 4);
        let p = v.get(off + child_ac as usize * 4..off + (child_ac + parent_ac) as usize * 4);
        let (c, p) = match (c, p) {
            (Some(c), Some(p)) => (c, p),
            _ => return false,
        };
        let cl = if child_ac > 2 { &c[(child_ac as usize - 2) * 4..] } else { c };
        let cv = read_cells(cl, child_ac.min(2));
        let pv = read_cells(p, parent_ac);
        if cv.is_none() || cv != pv {
            return false;
        }
        off += tuple;
    }
    true
}

/// A GIC interrupt specifier (`<type number flags>`, three cells).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GicIrq {
    /// 0 = SPI, 1 = PPI.
    pub kind: u32,
    /// Number within the class (SPI 0 is INTID 32, PPI 0 is INTID 16).
    pub number: u32,
    /// Trigger / polarity flags.
    pub flags: u32,
}

impl GicIrq {
    /// The GIC INTID: PPI `n` -> `n + 16`, SPI `n` -> `n + 32`; `None` for other classes.
    pub fn intid(&self) -> Option<u32> {
        match self.kind {
            0 => self.number.checked_add(32),
            1 => self.number.checked_add(16),
            _ => None,
        }
    }
}

/// GIC architecture version as far as the platform layer must distinguish it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GicVersion {
    /// GICv1/v2 (`arm,cortex-a15-gic`, `arm,gic-400`, ...): distributor plus a memory-mapped CPU
    /// interface.
    V2,
    /// GICv3/v4 (`arm,gic-v3`): distributor plus redistributors, system-register CPU interface.
    V3,
}

/// Maximum number of redistributor regions recorded.
pub const MAX_REDIST_REGIONS: usize = 4;

/// Description of the primary interrupt controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GicInfo {
    pub version: GicVersion,
    /// Distributor `(base, size)`.
    pub dist: (u64, u64),
    /// GICv2 CPU interface `(base, size)`; `None` on GICv3.
    pub cpu_if: Option<(u64, u64)>,
    /// GICv3 redistributor regions; only the first `redist_count` entries are valid.
    pub redist: [(u64, u64); MAX_REDIST_REGIONS],
    pub redist_count: usize,
    /// `redistributor-stride` (0 = frames are contiguous, the architectural default).
    pub redist_stride: u64,
    /// `arm,gic-v3-its` frame `(base, size)` if the tree has one.
    pub its: Option<(u64, u64)>,
}

/// The `arm,armv8-timer` node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerInfo {
    /// Secure physical, non-secure physical, virtual, hypervisor timer, in binding order.
    /// `None` where the tree has fewer than four interrupts.
    pub irqs: [Option<GicIrq>; 4],
    /// `clock-frequency`, if the tree states it (otherwise read `CNTFRQ_EL0`).
    pub frequency: Option<u32>,
}

impl TimerInfo {
    /// INTID of the non-secure EL1 physical timer, the one Caprock uses as its tick source.
    pub fn ns_phys_intid(&self) -> Option<u32> {
        self.irqs[1]?.intid()
    }
}

/// Pixel format of a `simple-framebuffer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbFormat {
    R5G6B5,
    R8G8B8,
    A8R8G8B8,
    X8R8G8B8,
    A8B8G8R8,
    X8B8G8R8,
    A2R10G10B10,
    X2R10G10B10,
    /// A format string this crate does not know.
    Other,
}

impl FbFormat {
    fn parse(s: &str) -> FbFormat {
        match s {
            "r5g6b5" => FbFormat::R5G6B5,
            "r8g8b8" => FbFormat::R8G8B8,
            "a8r8g8b8" => FbFormat::A8R8G8B8,
            "x8r8g8b8" => FbFormat::X8R8G8B8,
            "a8b8g8r8" => FbFormat::A8B8G8R8,
            "x8b8g8r8" => FbFormat::X8B8G8R8,
            "a2r10g10b10" => FbFormat::A2R10G10B10,
            "x2r10g10b10" => FbFormat::X2R10G10B10,
            _ => FbFormat::Other,
        }
    }

    /// Bits per pixel, `None` for [`FbFormat::Other`].
    pub fn bits_per_pixel(self) -> Option<u32> {
        match self {
            FbFormat::R5G6B5 => Some(16),
            FbFormat::R8G8B8 => Some(24),
            FbFormat::Other => None,
            _ => Some(32),
        }
    }
}

/// A `simple-framebuffer` node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FbInfo {
    pub base: u64,
    /// Size of the `reg` window in bytes.
    pub size: u64,
    pub width: u32,
    pub height: u32,
    /// Bytes per scanline.
    pub stride: u32,
    pub format: FbFormat,
}

/// Where a reserved-memory range came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReservedSource {
    /// The FDT header's memory reservation block.
    Header,
    /// A child of `/reserved-memory` with a static `reg`.
    Node,
}

/// One reserved physical range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reserved {
    pub base: u64,
    pub size: u64,
    /// The node carries `no-map` (the OS must not even map it).
    pub no_map: bool,
    pub source: ReservedSource,
}

/// A generic PCIe host (`pci-host-ecam-generic` or `pci-host-cam-generic`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciHostInfo {
    /// Configuration space window `(base, size)`.
    pub ecam: (u64, u64),
    /// `bus-range` (first, last); `(0, 255)` if absent.
    pub bus_range: (u32, u32),
    /// First 32-bit memory window: CPU `(base, size)` (PCI address assumed identical).
    pub mmio32: Option<(u64, u64)>,
    /// First 64-bit memory window: CPU `(base, size)`.
    pub mmio64: Option<(u64, u64)>,
}

impl<'a> Dtb<'a> {
    /// Visit every node in document order. The callback returns `true` to continue and `false`
    /// to stop early. `None` if the tree is malformed or deeper than [`MAX_DEPTH`].
    pub fn walk<F: FnMut(&Node<'a>) -> bool>(&self, mut f: F) -> Option<()> {
        let data = self.data;
        let mut pos = self.off_struct;
        let mut depth = 0usize;
        let mut names: [&'a [u8]; MAX_DEPTH] = [&[]; MAX_DEPTH];
        // Cells / identity valid for the CHILDREN of the node at depth index `d - 1`.
        let mut below: [(u32, u32, bool); MAX_DEPTH + 1] = [(0, 0, true); MAX_DEPTH + 1];
        below[0] = (DEFAULT_ADDR_CELLS, DEFAULT_SIZE_CELLS, true);
        loop {
            let tok = be32(data, pos)?;
            pos += 4;
            match tok {
                FDT_BEGIN_NODE => {
                    let name = cstr(data, pos);
                    pos = pos.checked_add(align4(name.len() + 1))?;
                    if depth >= MAX_DEPTH {
                        return None;
                    }
                    names[depth] = name;
                    depth += 1;
                    // Properties precede children; find where they end.
                    let props = pos;
                    let mut p = pos;
                    loop {
                        match be32(data, p)? {
                            FDT_PROP => {
                                let len = be32(data, p + 4)? as usize;
                                p = p.checked_add(12)?.checked_add(align4(len))?;
                            }
                            FDT_NOP => p += 4,
                            _ => break,
                        }
                    }
                    let (pac, psc, pident) = below[depth - 1];
                    let mut node = Node {
                        data,
                        off_strings: self.off_strings,
                        name,
                        depth,
                        names,
                        parent_addr_cells: pac,
                        parent_size_cells: psc,
                        props,
                        props_end: p,
                        cpu_addressable: pident,
                    };
                    let own_ac = node.prop_u32("#address-cells").unwrap_or(DEFAULT_ADDR_CELLS);
                    let own_sc = node.prop_u32("#size-cells").unwrap_or(DEFAULT_SIZE_CELLS);
                    let ident = depth == 1
                        || (pident && ranges_identity(node.prop("ranges"), own_ac, pac, own_sc));
                    below[depth] = (own_ac, own_sc, ident);
                    // The node's own `reg` is in the parent's space, so `cpu_addressable`
                    // reflects the parent's identity, which is what `pident` is.
                    node.cpu_addressable = pident;
                    if !f(&node) {
                        return Some(());
                    }
                    pos = p;
                }
                FDT_END_NODE => {
                    if depth == 0 {
                        return None;
                    }
                    depth -= 1;
                }
                FDT_NOP => {}
                FDT_END => return Some(()),
                _ => return None,
            }
        }
    }

    /// First enabled node whose `compatible` lists `needle`.
    pub fn find_compatible(&self, needle: &str) -> Option<Node<'a>> {
        let mut found = None;
        self.walk(|n| {
            if n.is_enabled() && n.is_compatible(needle) {
                found = Some(*n);
                false
            } else {
                true
            }
        })?;
        found
    }

    /// Call `f` for every enabled node whose `compatible` lists `needle`. Returns the count.
    pub fn for_each_compatible<F: FnMut(&Node<'a>)>(&self, needle: &str, mut f: F) -> Option<usize> {
        let mut n = 0;
        self.walk(|node| {
            if node.is_enabled() && node.is_compatible(needle) {
                f(node);
                n += 1;
            }
            true
        })?;
        Some(n)
    }

    /// First node at `path` (`/soc@0/serial@894000`), regardless of `status`.
    pub fn find_path(&self, path: &str) -> Option<Node<'a>> {
        let mut found = None;
        self.walk(|n| {
            if n.path_is(path) {
                found = Some(*n);
                false
            } else {
                true
            }
        })?;
        found
    }

    /// Call `f(base, size)` for every `reg` entry of every `memory` node (a child of the root
    /// with `device_type = "memory"` or the name `memory[@...]`). Returns the number of entries.
    /// Regions with size 0 (some firmware fills them in later) are reported as they are.
    pub fn for_each_memory<F: FnMut(u64, u64)>(&self, mut f: F) -> Option<usize> {
        let mut n = 0;
        self.walk(|node| {
            let is_mem = node.depth() == 2
                && node.is_enabled()
                && (node.prop("device_type") == Some(b"memory\0")
                    || node.base_name() == b"memory");
            if is_mem {
                for i in 0..node.reg_count() {
                    if let Some((b, s)) = node.reg(i) {
                        f(b, s);
                        n += 1;
                    }
                }
            }
            true
        })?;
        Some(n)
    }

    /// Call `f` for every reserved range: first the FDT header's reservation block, then the
    /// children of `/reserved-memory` that carry a static `reg`. Returns the number reported.
    pub fn for_each_reserved<F: FnMut(Reserved)>(&self, mut f: F) -> Option<usize> {
        let mut n = 0;
        let off_rsv = be32(self.data, 16)? as usize;
        let mut p = off_rsv;
        loop {
            let base = be64(self.data, p)?;
            let size = be64(self.data, p.checked_add(8)?)?;
            if base == 0 && size == 0 {
                break;
            }
            f(Reserved { base, size, no_map: false, source: ReservedSource::Header });
            n += 1;
            p = p.checked_add(16)?;
        }
        self.walk(|node| {
            if node.depth() == 3 && node.names[1] == b"reserved-memory" && node.is_enabled() {
                for i in 0..node.reg_count() {
                    if let Some((base, size)) = node.reg(i) {
                        f(Reserved {
                            base,
                            size,
                            no_map: node.has_prop("no-map"),
                            source: ReservedSource::Node,
                        });
                        n += 1;
                    }
                }
            }
            true
        })?;
        Some(n)
    }

    /// The primary GIC: the first enabled node with a known GIC `compatible`.
    pub fn gic(&self) -> Option<GicInfo> {
        const V2: [&str; 7] = [
            "arm,cortex-a15-gic",
            "arm,cortex-a9-gic",
            "arm,cortex-a7-gic",
            "arm,gic-400",
            "arm,arm11mp-gic",
            "arm,arm1176jzf-devchip-gic",
            "arm,pl390",
        ];
        let mut info: Option<GicInfo> = None;
        self.walk(|node| {
            if !node.is_enabled() {
                return true;
            }
            let version = if node.is_compatible("arm,gic-v3") {
                GicVersion::V3
            } else if V2.iter().any(|c| node.is_compatible(c)) {
                GicVersion::V2
            } else {
                return true;
            };
            let dist = match node.reg(0) {
                Some(d) => d,
                None => return true,
            };
            let mut g = GicInfo {
                version,
                dist,
                cpu_if: None,
                redist: [(0, 0); MAX_REDIST_REGIONS],
                redist_count: 0,
                redist_stride: 0,
                its: None,
            };
            match version {
                GicVersion::V2 => g.cpu_if = node.reg(1),
                GicVersion::V3 => {
                    let regions = node.prop_u32("#redistributor-regions").unwrap_or(1) as usize;
                    if regions == 0 || regions > MAX_REDIST_REGIONS || node.reg_count() < 1 + regions {
                        return true;
                    }
                    for i in 0..regions {
                        match node.reg(1 + i) {
                            Some(r) => g.redist[i] = r,
                            None => return true,
                        }
                    }
                    g.redist_count = regions;
                    g.redist_stride = match node.prop("redistributor-stride") {
                        Some(v) => read_cells(v, (v.len() / 4).min(2) as u32).unwrap_or(0),
                        None => 0,
                    };
                }
            }
            info = Some(g);
            false
        })?;
        let mut g = info?;
        if g.version == GicVersion::V3 {
            g.its = self.find_compatible("arm,gic-v3-its").and_then(|n| n.reg(0));
        }
        Some(g)
    }

    /// The architected timer node (`arm,armv8-timer`, falling back to `arm,armv7-timer`).
    /// Interrupt specifiers are assumed to be GIC-style triplets.
    pub fn armv8_timer(&self) -> Option<TimerInfo> {
        let node = self
            .find_compatible("arm,armv8-timer")
            .or_else(|| self.find_compatible("arm,armv7-timer"))?;
        let v = node.prop("interrupts")?;
        if v.len() % 12 != 0 || v.is_empty() || v.len() > 48 {
            return None;
        }
        let mut irqs = [None; 4];
        for (i, slot) in irqs.iter_mut().enumerate().take(v.len() / 12) {
            *slot = Some(GicIrq {
                kind: be32(v, i * 12)?,
                number: be32(v, i * 12 + 4)?,
                flags: be32(v, i * 12 + 8)?,
            });
        }
        Some(TimerInfo { irqs, frequency: node.prop_u32("clock-frequency") })
    }

    /// The raw `stdout-path` string of `/chosen` (`linux,stdout-path` accepted too), without a
    /// trailing `:options` part.
    pub fn stdout_path(&self) -> Option<&'a str> {
        let mut out = None;
        self.walk(|n| {
            if n.depth() == 2 && n.name() == b"chosen" {
                out = n.prop_str("stdout-path").or_else(|| n.prop_str("linux,stdout-path"));
                false
            } else {
                true
            }
        })?;
        let s = out?;
        Some(s.split(':').next().unwrap_or(s))
    }

    /// The node `/chosen/stdout-path` points to. The path may be absolute or an alias from
    /// `/aliases`.
    pub fn stdout_node(&self) -> Option<Node<'a>> {
        let sp = self.stdout_path()?;
        if sp.starts_with('/') {
            return self.find_path(sp);
        }
        let mut target = None;
        self.walk(|n| {
            if n.depth() == 2 && n.name() == b"aliases" {
                target = n.prop_str(sp);
                false
            } else {
                true
            }
        })?;
        self.find_path(target?)
    }

    /// Call `f` for every enabled `simple-framebuffer` with complete geometry.
    pub fn for_each_framebuffer<F: FnMut(FbInfo)>(&self, mut f: F) -> Option<usize> {
        let mut n = 0;
        self.for_each_compatible("simple-framebuffer", |node| {
            let (base, size) = match node.reg(0) {
                Some(r) => r,
                None => return,
            };
            let (w, h, s) = match (node.prop_u32("width"), node.prop_u32("height"), node.prop_u32("stride")) {
                (Some(w), Some(h), Some(s)) => (w, h, s),
                _ => return,
            };
            let format = FbFormat::parse(node.prop_str("format").unwrap_or(""));
            f(FbInfo { base, size, width: w, height: h, stride: s, format });
            n += 1;
        })?;
        Some(n)
    }

    /// The first generic PCIe ECAM host.
    pub fn pci_host(&self) -> Option<PciHostInfo> {
        let node = self
            .find_compatible("pci-host-ecam-generic")
            .or_else(|| self.find_compatible("pci-host-cam-generic"))?;
        let ecam = node.reg(0)?;
        let bus_range = match node.prop("bus-range") {
            Some(v) if v.len() == 8 => (be32(v, 0)?, be32(v, 4)?),
            _ => (0, 255),
        };
        let mut host = PciHostInfo { ecam, bus_range, mmio32: None, mmio64: None };
        // `ranges`: <pci.hi pci.mid pci.lo  cpu(parent_ac)  size(2)> ; space code in pci.hi.
        let (pac, _) = node.parent_cells();
        let pci_ac = node.prop_u32("#address-cells").unwrap_or(3);
        let pci_sc = node.prop_u32("#size-cells").unwrap_or(2);
        if pci_ac == 3 && pci_sc <= 2 && pac <= 2 {
            if let Some(v) = node.prop("ranges") {
                let tuple = (3 + pac + pci_sc) as usize * 4;
                let mut off = 0;
                while off + tuple <= v.len() {
                    let hi = be32(v, off)?;
                    let cpu = read_cells(v.get(off + 12..)?, pac)?;
                    let size = read_cells(v.get(off + 12 + pac as usize * 4..)?, pci_sc)?;
                    match (hi >> 24) & 3 {
                        2 if host.mmio32.is_none() => host.mmio32 = Some((cpu, size)),
                        3 if host.mmio64.is_none() => host.mmio64 = Some((cpu, size)),
                        _ => {}
                    }
                    off += tuple;
                }
            }
        }
        Some(host)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    // Real blobs. QEMU: `qemu-system-aarch64 -machine virt,iommu=smmuv3 -cpu cortex-a72 -smp 8
    // -m 4G,dumpdtb=...` then `dtc -I dtb -O dtb`. Qualcomm: the mainline x1p42100 Vivobook S15
    // DTS compiled with cpp + dtc. Synthetic: `synthetic-fb.dts` next to the blobs.
    static QEMU: &[u8] = include_bytes!("../tests/fixtures/qemu-virt-smmu-8cpu.dtb");
    static EMBEDDED_VIRT: &[u8] = include_bytes!("../../../kernel/src/virt.dtb");
    static QCOM: &[u8] = include_bytes!("../tests/fixtures/x1p42100-asus-vivobook-s15.dtb");
    static SYNTH: &[u8] = include_bytes!("../tests/fixtures/synthetic-fb.dtb");

    fn dtb(b: &[u8]) -> Dtb<'_> {
        Dtb::parse(b).expect("fixture parses")
    }

    #[test]
    fn qemu_memory_cpus_unchanged() {
        for blob in [QEMU, EMBEDDED_VIRT] {
            let d = dtb(blob);
            assert_eq!(d.memory(), Some((0x4000_0000, 0x1_0000_0000)));
            assert!(d.cpu_count().unwrap() >= 1);
        }
        assert_eq!(dtb(QEMU).cpu_count(), Some(8));
    }

    #[test]
    fn qemu_gic_v2() {
        for blob in [QEMU, EMBEDDED_VIRT] {
            let g = dtb(blob).gic().unwrap();
            assert_eq!(g.version, GicVersion::V2);
            assert_eq!(g.dist, (0x0800_0000, 0x1_0000));
            assert_eq!(g.cpu_if, Some((0x0801_0000, 0x1_0000)));
            assert_eq!(g.redist_count, 0);
            assert_eq!(g.its, None);
        }
    }

    #[test]
    fn qemu_timer_ns_phys_is_intid_30() {
        let t = dtb(QEMU).armv8_timer().unwrap();
        assert_eq!(t.ns_phys_intid(), Some(30));
        assert_eq!(t.irqs[0].unwrap().intid(), Some(29));
        assert_eq!(t.irqs[2].unwrap().intid(), Some(27));
        assert_eq!(t.irqs[3].unwrap().intid(), Some(26));
        assert_eq!(t.frequency, None);
    }

    #[test]
    fn qemu_console_is_pl011() {
        for blob in [QEMU, EMBEDDED_VIRT] {
            let d = dtb(blob);
            assert_eq!(d.stdout_path(), Some("/pl011@9000000"));
            let n = d.stdout_node().unwrap();
            assert!(n.is_compatible("arm,pl011"));
            assert_eq!(n.reg(0), Some((0x0900_0000, 0x1000)));
            assert!(n.cpu_addressable());
        }
    }

    #[test]
    fn qemu_pcie_ecam_and_windows() {
        let p = dtb(QEMU).pci_host().unwrap();
        assert_eq!(p.ecam, (0x40_1000_0000, 0x1000_0000));
        assert_eq!(p.bus_range, (0, 255));
        assert_eq!(p.mmio32, Some((0x1000_0000, 0x2eff_0000)));
        assert_eq!(p.mmio64, Some((0x80_0000_0000, 0x80_0000_0000)));
    }

    #[test]
    fn qemu_virtio_mmio_32_slots() {
        let mut v: Vec<(u64, u64)> = Vec::new();
        let n = dtb(QEMU).for_each_compatible("virtio,mmio", |n| v.push(n.reg(0).unwrap())).unwrap();
        assert_eq!(n, 32);
        assert_eq!(v[0], (0x0a00_0000, 0x200));
        assert_eq!(v[31], (0x0a00_3e00, 0x200));
    }

    #[test]
    fn non_identity_ranges_are_flagged() {
        let n = dtb(QEMU).find_compatible("qemu,platform").unwrap();
        assert_eq!(n.parent_cells(), (2, 2));
        // Its own reg lives in root space; children would be shifted by `ranges`.
        assert!(n.cpu_addressable());
        let soc = dtb(SYNTH).find_path("/soc/uart@10000000").unwrap();
        assert!(soc.cpu_addressable(), "empty `ranges;` is the identity");
        let g = dtb(SYNTH).find_compatible("x,dev").unwrap();
        assert!(!g.cpu_addressable(), "non-identity ranges must be flagged");
        assert_eq!(g.reg(0), Some((0x10, 0x10)));
    }

    #[test]
    fn qemu_no_reserved_no_framebuffer() {
        let d = dtb(QEMU);
        assert_eq!(d.for_each_reserved(|_| {}), Some(0));
        assert_eq!(d.for_each_framebuffer(|_| {}), Some(0));
    }

    #[test]
    fn qcom_gic_v3_redistributors_and_its() {
        let g = dtb(QCOM).gic().unwrap();
        assert_eq!(g.version, GicVersion::V3);
        assert_eq!(g.dist, (0x1700_0000, 0x1_0000));
        assert_eq!(g.cpu_if, None);
        assert_eq!(g.redist_count, 1);
        assert_eq!(g.redist[0], (0x1708_0000, 0x30_0000));
        assert_eq!(g.redist_stride, 0x4_0000);
        assert_eq!(g.its, Some((0x1704_0000, 0x4_0000)));
    }

    #[test]
    fn qcom_timer_and_cells() {
        let t = dtb(QCOM).armv8_timer().unwrap();
        assert_eq!(t.ns_phys_intid(), Some(30));
        assert_eq!(t.irqs[1].unwrap().flags, 8);
        // soc@0 uses 2/2 cells with an identity `ranges`.
        let uart = dtb(QCOM).find_path("/soc@0/geniqup@ac0000/serial@a98000").unwrap();
        assert!(uart.is_compatible("qcom,geni-uart") || uart.is_compatible("qcom,geni-debug-uart"));
        assert_eq!(uart.reg(0), Some((0xa98000, 0x4000)));
        assert!(uart.cpu_addressable());
    }

    #[test]
    fn qcom_memory_is_firmware_filled_and_reserved_is_listed() {
        let d = dtb(QCOM);
        let mut mem = Vec::new();
        d.for_each_memory(|b, s| mem.push((b, s))).unwrap();
        assert_eq!(mem, [(0x8000_0000, 0)]);
        assert_eq!(d.memory(), None, "an empty region is not usable RAM");
        let mut r = Vec::new();
        let n = d.for_each_reserved(|x| r.push(x)).unwrap();
        assert_eq!(n, r.len());
        assert!(r.len() > 30);
        let first = r.iter().find(|x| x.source == ReservedSource::Node).unwrap();
        assert_eq!((first.base, first.size, first.no_map), (0x8000_0000, 0x80_0000, true));
    }

    #[test]
    fn qcom_has_no_stdout_and_no_framebuffer() {
        let d = dtb(QCOM);
        assert_eq!(d.stdout_path(), None);
        assert!(d.stdout_node().is_none());
        assert_eq!(d.for_each_framebuffer(|_| {}), Some(0));
    }

    #[test]
    fn synthetic_memory_two_regions_one_cell() {
        let d = dtb(SYNTH);
        let mut mem = Vec::new();
        d.for_each_memory(|b, s| mem.push((b, s))).unwrap();
        assert_eq!(mem, [(0x4000_0000, 0x1000_0000), (0x6000_0000, 0x0800_0000)]);
        assert_eq!(d.memory(), Some((0x4000_0000, 0x1000_0000)));
    }

    #[test]
    fn synthetic_reserved_header_and_node_skip_dynamic() {
        let mut r = Vec::new();
        let n = dtb(SYNTH).for_each_reserved(|x| r.push(x)).unwrap();
        assert_eq!(n, 2, "the dynamic `pool` (size only) has no static range");
        assert_eq!(
            r[0],
            Reserved { base: 0x4e00_0000, size: 0x10_0000, no_map: false, source: ReservedSource::Header }
        );
        assert_eq!(
            r[1],
            Reserved { base: 0x4f00_0000, size: 0x1000, no_map: true, source: ReservedSource::Node }
        );
    }

    #[test]
    fn synthetic_framebuffer() {
        let mut fb = Vec::new();
        dtb(SYNTH).for_each_framebuffer(|f| fb.push(f)).unwrap();
        assert_eq!(
            fb,
            [FbInfo {
                base: 0x8000_0000,
                size: 0x7e_9000,
                width: 1920,
                height: 1080,
                stride: 7680,
                format: FbFormat::A8R8G8B8
            }]
        );
        assert_eq!(fb[0].format.bits_per_pixel(), Some(32));
    }

    #[test]
    fn synthetic_stdout_alias_with_options_and_disabled_skipped() {
        let d = dtb(SYNTH);
        assert_eq!(d.stdout_path(), Some("serial0"));
        let n = d.stdout_node().unwrap();
        assert_eq!(n.reg(0), Some((0x1000_0000, 0x100)));
        // The disabled UART is invisible to compatible lookup.
        assert_eq!(d.for_each_compatible("ns16550a", |_| {}), Some(1));
        // Path matching tolerates a missing unit address.
        assert!(n.path_is("/soc/uart@10000000"));
        assert!(!n.path_is("/soc/uart@10001000"));
    }

    #[test]
    fn synthetic_gic400_and_three_irq_timer() {
        let d = dtb(SYNTH);
        let g = d.gic().unwrap();
        assert_eq!(g.version, GicVersion::V2);
        assert_eq!(g.dist, (0x2c00_1000, 0x1000));
        assert_eq!(g.cpu_if, Some((0x2c00_2000, 0x2000)));
        let t = d.armv8_timer().unwrap();
        assert_eq!(t.irqs[3], None);
        assert_eq!(t.ns_phys_intid(), Some(30));
        assert_eq!(t.frequency, Some(19_200_000));
    }

    #[test]
    fn no_pci_host_in_synthetic() {
        assert!(dtb(SYNTH).pci_host().is_none());
    }

    #[test]
    fn truncation_never_panics() {
        // Every prefix of every blob must yield None or a value, never a panic or OOB.
        for blob in [QEMU, SYNTH, EMBEDDED_VIRT] {
            let mut cut = 0;
            while cut < blob.len() {
                if let Some(d) = Dtb::parse(&blob[..cut]) {
                    let _ = d.memory();
                    let _ = d.gic();
                    let _ = d.armv8_timer();
                    let _ = d.stdout_node();
                    let _ = d.pci_host();
                    let _ = d.for_each_reserved(|_| {});
                    let _ = d.for_each_framebuffer(|_| {});
                    let _ = d.cpu_count();
                }
                cut += if cut < 600 { 1 } else { 97 };
            }
        }
    }

    #[test]
    fn corrupted_bytes_never_panic() {
        let mut v = SYNTH.to_vec();
        for i in (40..v.len()).step_by(3) {
            let old = v[i];
            v[i] = 0xff;
            if let Some(d) = Dtb::parse(&v) {
                let _ = d.memory();
                let _ = d.gic();
                let _ = d.stdout_node();
                let _ = d.for_each_reserved(|_| {});
            }
            v[i] = old;
        }
    }

    #[test]
    fn bad_magic_rejected() {
        assert!(Dtb::parse(&[0u8; 64]).is_none());
        assert!(Dtb::parse(&[]).is_none());
    }
}
