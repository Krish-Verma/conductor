# ADR-0011 — the credential boundary had two holes the environment list cannot close

**Status:** ACCEPTED
**Date:** 2026-08-15
**Slice:** S9 (found by writing the test §4.9 had never had)

---

## Question

§4.9 makes credential absence **layer 6, the primary control**:

> *An agent with no push credential cannot push, regardless of what it types,
> what it is told, or whether any hook fires.*

and enumerates the mechanism:

> `GIT_TERMINAL_PROMPT=0`; `GIT_ASKPASS` → a binary that always exits non-zero.
> `SSH_AUTH_SOCK` unset; no `~/.netrc`; no `GH_TOKEN`/`GITHUB_TOKEN`; no cloud or
> database variables. A **per-run `HOME`**, so `~/.aws`, `~/.config/gh`,
> `~/.kube` are simply absent.

S9 is the slice that must write `SECURITY.md`, and the rule for that document is
that **no item may be listed as prevented without a passing test**. Writing the
test asked a question the implementation had never been asked: *does each named
mechanism actually exist on the host, and is the environment the only way in?*

## Why the answer matters to Conductor

Layer 6 is load-bearing precisely because the layers above it are not. Layer 1
(prompt instructions) is worth zero by the master plan's own table; layer 4
(hooks) has known bypasses; layer 5 (OS sandbox) is Codex-only. If layer 6 has a
hole, the honest security table has almost nothing left in the "prevented"
column, and every claim resting on "the agent has no credential" is unproven.

## Experiment / evidence

Three measurements on the host (macOS 25.6.0, Xcode-provided git 2.51.0), all
with disposable canaries — no real secret value was read.

**1. Does the named askpass program exist?**

```
$ ls -la /bin/false
ls: /bin/false: No such file or directory
$ ls -la /usr/bin/false
-rwxr-xr-x  1 root  wheel  84032 /usr/bin/false
```

`crates/conductor-run/src/worker.rs` had shipped
`env.insert("GIT_ASKPASS", "/bin/false")` since S5. On macOS that path does not
exist. Git reports:

```
fatal: cannot exec '/bin/false': No such file or directory
fatal: could not read Username for 'https://github.com': terminal prompts disabled
```

**2. Does a credential source survive `env_clear` and a redirected `HOME`?**

```
$ env -i PATH=/usr/bin:/bin HOME=<disposable> git config --list --show-origin
file:/Applications/Xcode.app/.../usr/share/git-core/gitconfig  credential.helper=osxkeychain
```

A **system-scoped** gitconfig is located by absolute path. It is unaffected by
clearing the environment and unaffected by pointing `HOME` somewhere else.

**3. Does the mitigation work?**

```
$ env -i PATH=/usr/bin:/bin HOME=<disposable> GIT_CONFIG_NOSYSTEM=1 \
      GIT_TERMINAL_PROMPT=0 git credential fill  <<< 'protocol=https\nhost=github.com\n\n'
fatal: could not read Username for 'https://github.com': terminal prompts disabled
```

## Observed result

Two distinct defects, of different severity.

**(a) The askpass mechanism was absent, and failed safe by accident.**
Because `GIT_TERMINAL_PROMPT=0` catches the fallback, no credential was ever
obtainable and no behaviour was wrong. But the *named* mechanism did not exist,
and `SECURITY.md` was about to name it. A control that is documented, believed,
and not present is the exact failure mode this slice exists to eliminate — it
survived four slices because nothing ever asserted the file was there.

**(b) The system gitconfig is a real credential path the environment cannot
close.** On this host it declares `credential.helper=osxkeychain`. An agent that
runs `git remote add origin https://github.com/...` followed by `git push` would
have git consult the keychain — no environment variable involved, so every
control in §4.9's list is irrelevant to it.

On *this* host the keychain holds no `github.com` credential, so the hole was not
live. That is a property of the operator's machine, not of Conductor, and a
control whose correctness depends on the operator's keychain being empty is not a
control.

## Decision

1. **Conductor writes its own askpass program**, into the per-run `HOME`, at mode
   `0500`. It is `#!/bin/sh` + `exit 1`, prints nothing on stdout (git reads
   stdout as the credential), and is rewritten on every `prepare` so an agent
   that replaced it does not get to keep the replacement. Its existence is
   asserted by `the_askpass_program_exists_and_always_fails`, whose `.is_file()`
   check is the assertion that would have caught the original defect.

2. **`GIT_CONFIG_NOSYSTEM=1` joins the allowlist**, with `GIT_CONFIG_GLOBAL` and
   `GIT_CONFIG_SYSTEM` pinned to `/dev/null` as belt and braces for git versions
   that consult them. `the_system_gitconfig_cannot_be_read` asserts no
   `credential.helper` is reachable, and includes a positive control that drops
   the mitigation and requires the setting to reappear — so the test cannot pass
   on a host that never had the problem without saying so out loud.

3. **The environment can only be obtained by `prepare`**, which does the I/O.
   The related S5 defect — `HOME` and `TMPDIR` named directories nobody created
   — is closed by construction rather than by remembering to `mkdir`: the type
   has no other constructor.

## What this DOES prove

- On this host, under the S9 environment, `git` cannot obtain a credential by
  prompting, by askpass, by `$HOME`-relative discovery, or from any
  system-scoped config, and each of those has a test with a positive control.
- The four controls have teeth: removing `GIT_CONFIG_NOSYSTEM`, reverting the
  askpass to `/bin/false`, skipping the `TMPDIR` `mkdir`, and inheriting a single
  `GH_TOKEN` were each applied as mutations and each was caught by a named test.

## What this DOES NOT prove

- **Nothing here contains reads.** M12 stands: under the measured sandbox an
  agent can still read `~/.ssh` or `~/.codex/auth.json` by absolute path. The
  per-run `HOME` defeats *discovery*, not *reading a path you already know*.
- **It is not a claim about other hosts.** A Linux host with `/etc/gitconfig`,
  or a machine with a different credential helper, has a different starting
  position. The mitigation is general; the measurement is of this host.
- **It says nothing about non-git credential paths.** A tool that reads a
  hardcoded absolute path, or an agent binary with a credential compiled in, is
  outside what an environment allowlist can address.
- **`PATH` is inherited by value.** Every binary on the operator's `PATH` is
  reachable. This is deliberate — an agent that cannot run tools is useless — and
  it means "allowlisted environment" is not "empty environment".

## Pre-registered falsification / revisit trigger

- A git release that reads a credential helper from a source none of
  `GIT_CONFIG_NOSYSTEM`, `GIT_CONFIG_GLOBAL` or `GIT_CONFIG_SYSTEM` suppresses.
  `the_system_gitconfig_cannot_be_read` fails and this record is revisited.
- Any adapter that requires an inherited credential variable to function. It must
  arrive through `RunEnvironment::with_extra` **by name**, and the allowlist test
  will show it as an extra key rather than letting it in silently.
- Supporting a platform whose `/bin/sh` is absent would break the askpass
  program; the `.is_file()` assertion catches it rather than failing open.

## Impacted master-plan sections

- **§4.9** — the environment list gains `GIT_CONFIG_NOSYSTEM`,
  `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`, and the askpass is described as
  Conductor-written rather than as a host path.
- **Part 8, S9** — `enforce/env.rs` is the owner; the per-run `HOME`/`TMPDIR`
  are created rather than merely named.
- **`SECURITY.md`** — "credential read" remains `NOT PREVENTED`; "credential
  *discovery* through git" becomes `PREVENTED`, with these tests named.
