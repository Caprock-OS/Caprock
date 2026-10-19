# Additional permission: the ABI exception

**Not legal advice.** This is the structure of the rule as it is generally understood. Before a
product or distribution decision it belongs in front of someone who is licensed to practise.

Caprock is licensed under `AGPL-3.0-or-later` (see `LICENSE`). This document grants an
**additional permission** within the meaning of (A)GPLv3 §7 — it takes **nothing** away and restricts nobody.

---

## The exception

> A standalone program that communicates with the kernel exclusively via the **published
> system interface** of Caprock is, for that reason alone, **not** a derivative work of the kernel,
> and its distribution is, for that reason alone, **not** subject to the GPL.
>
> The published system interface comprises: the syscall numbers, result codes and
> register assignments from `crates/caprock-abi`, the IPC message formats, the manifest format,
> the service protocols of the bundled PDs — as well as the **boundary crates** (see `docs/grenze.md`),
> insofar as they are under a permissive licence.
>
> The exception does **not** extend to programs that contain kernel code, statically link it
> or modify it.

## Scope: general

**The exception applies to everyone, without limit, without application and without distinction by the purpose of
use** — commercial and non-commercial alike.

This is a deliberate decision and not the omission of a restriction. An exception that
applied only to part of the users would have three drawbacks that undermine its purpose:

* **It would have forbidden nothing.** GPLv3 §7 allows additional *permissions* and forbids
  additional *restrictions*. The GPLv3 permits commercial use anyway; a
  restricted exception would not have prohibited it, but merely withheld the
  **clarification**.
* **It would have created uncertainty where certainty is the whole point.** Whoever first has to check whether
  their program falls under the exception has exactly the problem the exception is meant to remove.
* **"Commercial" is not sharp** — and vagueness in a permission is at the expense of whoever grants it,
  not of whoever relies on it.

The model is the same construction Linux chose for its system interface: the
exception applies to all, and the copyleft character of the kernel remains untouched.

## What the exception does NOT do

* It changes **nothing** about the GPLv3 for Caprock itself. Whoever modifies the kernel or core-adjacent crates
  and passes them on, passes them on under the GPLv3.
* It makes **no** statement about programs that contain or statically link kernel code.
* It is **no** statement about foreign code: a Linux compatibility layer contains
  Linux code and is thus a derivative work of the **Linux** kernel — the GPLv2 applies to that, and
  this exception changes nothing about it (see the separate repo `caprock-linux-compat`).

---

## Two consequences that belong to the document

**First: only whoever holds the rights can grant an exception.** As long as that is one person, it is
a commit. With the first merged outside contribution, someone else holds them too — and from then on
neither can one relicense nor change an exception without asking everyone. If a CLA or
DCO is ever to come, the time is **before** the first external contribution. The same logic as with the
signed manifest surface: the rule must stand before the first case occurs.

**Second: under the AGPL the copyleft applies ALSO on operation — and precisely for that reason this
exception is now load-bearing instead of merely convenient.**

`AGPL-3.0` §13 ("Remote Network Interaction") requires: whoever operates a **modified** version
and offers it over a network must offer the users the **source of that version**. The
duty thus arises on **operation**, not only on distribution — that is the difference
from the GPLv3 and the reason for the switch.

What this concretely separates:

| | does §13 apply |
|---|---|
| someone **modifies** the kernel and operates it as a service | **yes** — source to the users |
| someone operates Caprock **unmodified** as a service | no (nothing modified) |
| a customer runs their **own program** as a PD on it | **no** — it is not a derivative work, see the exception above |

**The third row is the reason the ABI exception is now load-bearing.** Without it, for
every customer the question would hang in the air whether their workload is caught by §13 — and a PaaS whose
customers first have to ask a lawyer whether their code must be disclosed has no
licence problem but a sales problem. With the exception the answer is one sentence.

**The price that comes with it:** quite a few companies exclude AGPL software across the board, without examining
the distinction above. That is a real cost side of the choice and not a legal question.
