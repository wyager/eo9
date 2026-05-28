# User study 06 — junior developer / CS student (the getting-started experience)

## Session metadata

- **Date:** 2026-05-27
- **Branch / worktree:** `study/session-novice` (worktree of master at `c71ded7`)
- **Participant persona:** a junior developer / CS student, ~1–2 years of experience
  (comfortable with basic terminal use, Python, a bit of JavaScript; has never written Rust;
  has never heard of WebAssembly components, WIT, or capability security; gets discouraged
  by jargon and by errors they cannot interpret).
- **Session focus:** the getting-started experience — follow the README literally, as a new
  user would, and record where a newcomer gets lost.
- **Methodology:** the participant was a role-played persona run as a separate session with
  no access to the repository, its documentation, or any tools — it saw only what the
  facilitator pasted into the conversation and replied conversationally. Every command shown
  to the participant was actually executed by the facilitator in the study environment;
  outputs are verbatim, trimmed only for length. Failures and breakage were shown as they
  happened, not cleaned up.
- **Environment:** Apple Silicon macOS host. The `eo9` binary was built from the worktree
  exactly as the README says (`cargo install --path crates/eo9`), with two recorded
  deviations made for hygiene, both disclosed to the participant: the install went to a
  study-local `--root` prefix (so the host's own `~/.cargo/bin/eo9` was not clobbered), and
  `EO9_STORE` pointed at an empty throwaway directory (so the session saw what a brand-new
  machine would see, instead of the host's already-seeded store). A throwaway `sandbox/`
  directory was used for `--fs-root`. Interactive shell segments were driven by piping
  command lines over stdin. The host already had the pinned nightly toolchain, the
  `wasm32-unknown-unknown` target, and `wasm-tools` installed, so those potential first-day
  hurdles were not observed in this session.
- **Shape:** nine facilitator rounds — pitch → install + first run → recovery →
  README examples + first denial → the `only` algebra + coreutils → sandbox/overlay
  questions + authoring expectations → write-your-own program → substitution demo →
  structured wrap-up.

## Round 1 — the pitch

The participant was given the README's own framing (capability-secure OS on the WebAssembly
Component Model; "a program's imports are its permissions"; "granting and revoking authority
is an algebra"; deny by default; everything is a component; same components in userspace and
on bare metal) plus the two-command getting-started block, and asked for a first reaction.

Their reaction, condensed: "That pitch reads like it's written for somebody who already
knows all the words." What landed: "operating system in Rust", and the two-command install
("I've done pip-install-type stuff plenty"). What did not: "Component Model" ("does this run
in a browser? On my laptop? Both?"), "capability-secure" / "no ambient authority" ("jargon
I'd have to google" — best guess was phone-app permissions), "granting authority is an
algebra" ("that sentence means nothing to me"), "everything is a component" ("circular,
because I don't know what a component is"). They asked to just run the two commands and see
what happens, then asked for "a one-paragraph plain-English explanation of what 'a program's
imports are its permissions' means with a concrete example, because right now that's the
whole pitch and it's bouncing off me."

## Round 2 — the install and the first run (the README as written)

`cargo install --path crates/eo9` from the fresh worktree: 239 packages locked, 202 crates
compiled, finished cleanly in 1m 17s, one PATH warning. The participant: "step 1 actually
went better than I expected."

Step 2, exactly as the README promises ("the first run seeds the store with the bundled
programs, then drops you at an `eosh>` prompt"):

```
$ eo9
eo9: error: cannot find the eosh component: bind it in the store (`eo9 store add
<path-to-eosh.wasm> --name eosh`) or build it in a development tree (`cargo xtask
build-guest`, which produces guest/target/components/eosh.wasm), then run `eo9 shell` again
(exit code 3)

$ eo9 hello --name world --excited true
eo9: error: name hello does not resolve in profile "default"
(exit code 3)
```

Participant reaction: "that's a direct contradiction of the README, right? … If I were on my
own laptop I'd 100% assume *I* did something wrong, because the README said this should Just
Work." Going through the error words: "component" — guessed "the program file?"; "store" —
"is that like a database? A folder? An app store?"; "bind" — "why not just 'add'?"; the
suggested `store add` command "wants a path to eosh.wasm and I have no idea where that file
would be — the README said the programs were *bundled*. If they're bundled… where are they?";
"xtask" and "guest" — never seen, "that word is doing a lot of work and I don't know what it
means here"; "profile" — "a new thing the README never mentioned. So now there are stores AND
profiles and I haven't successfully run anything yet." They said they would probably try
`cargo xtask build-guest` because the error suggested it — "honestly though, real talk,
there's a decent chance I'd just close the terminal at this point." They asked whether the
README explains the store anywhere, and whether a truly fresh user would hit this too.

## Round 3 — the recovery, and the cwd-dependent behavior

Facilitator answers given: a truly fresh user hits exactly this — the bundled programs are
embedded into the `eo9` binary at build time, but only if `cargo xtask build-guest` has
already produced them, and the README's userspace section never mentions that step, so the
README's own install order yields a binary with nothing inside it. The word "store" appears
exactly once in the README (in the comment the participant had already seen) and is never
defined.

`cargo xtask build-guest` was run as the error suggested: 267 crates compiled, 36 components
produced, 39.5s, clean. After that:

- `eo9` **from inside the checkout** now boots the shell (the dev-tree fallback finds
  `guest/target/components/` relative to the current directory); `help` and
  `hello --name world --excited true` work at the `eosh>` prompt (`ok: greeted`).
- `eo9 hello --name world --excited true` run directly **still fails** ("name hello does not
  resolve in profile \"default\"") even from the checkout root, because direct runs resolve
  via the (still empty) store only.
- From any other directory, `eo9` still fails with the original "cannot find the eosh
  component" error.

Participant reaction: "the README is broken for literally everyone, not just this setup …
On my own laptop I would never have figured out the fix was 'build this other thing, then
*reinstall the thing you already installed*.'" The directory-dependence "would have
absolutely melted my brain … that's the kind of invisible behavior that makes me think I
broke something." On the shell `help` text: `program --flag value`, `help`, `history`,
`exit`, `let` were fine; `provider $ program` is "clearly the *whole point* of the tool and I
don't understand it. What's a 'provider'? Why is the operator a `$`?"; `base & layer`,
`only`, `rename`, `with … as <slot>` were "all noise to me right now. 'Capability slot'
especially"; `env` / `describe` / `imports` "sound like the 'explain to me what's going on'
commands, which I'd probably lean on a lot." They also asked what the giant number in front
of "Hello, world!" was, and whether `ok: greeted` came from the program or the shell. They
asked next for: the promised path working end to end from a random folder, `describe hello`,
and a real denial.

## Round 4 — the working happy path, inspection, and the first denial

After re-running the install (9.8s the second time — only the changed crate rebuilt), from a
non-repo directory:

```
$ eo9
eo9: first run: seeded 36 bundled programs into the module store at …/store
eosh — the Eo9 shell (type `help`)

$ eo9 hello --name world --excited true
[1779929351.826967000] Hello, world!
success(greeted)

$ eo9 cruncher --seed 9 --rounds 200000
success(digest(14341732361190694547))
```

`describe hello` (args `--name`/`--excited`, imports text + time) and `env hello` (each
import marked "satisfied by the session" or "always available (carries no authority)") were
shown, plus the README's deny-by-default example:

```
$ eo9 readwrite --path note.txt --contents hi
eo9: error: readwrite (store object 20d5b29c…) requires the eo9:fs filesystem capability,
which eo9 does not grant by default: pass `--fs-root <dir>` to give the program access to a
host directory (guest paths cannot escape that root)            (exit code 3)

$ eo9 --fs-root ./sandbox readwrite --path note.txt --contents hi
success(round-tripped(2))                                        (exit code 0)
```

Participant reaction: "this is the first stretch where I actually feel like I get what the
tool is *for*." The readwrite refusal "is genuinely the best [error message] so far: it tells
me what's missing, why, and the exact flag to fix it. If the eosh error from earlier had been
written like that, this whole session would've gone smoother." The describe/env translation
landed. New questions: (1) "if it's 'deny by default, all the way down,' why does hello get
text and time *by default*? … right now it feels a little like 'deny by default, except for
the stuff we decided is fine'" — is there a list somewhere? (2) `success(greeted)` vs
`ok: greeted` "would trip me up if I were grepping output or following a tutorial."
(3) does the program see `--fs-root` as its whole filesystem? They asked next for the
take-the-clock-away-from-hello denial and a look at the bundled ls/cat.

## Round 5 — the `only` algebra, and the coreutils

The default-grant question was answered candidly (direct runs get terminal text, the host
clock, and host randomness wired by default; fs only via `--fs-root`; no network exists; the
list lives in the launcher's code, not the README; "the README's blanket 'no ambient
authority' sentence oversells it").

Withholding hello's clock — the README's own shorthand first, then the working form:

```
$ eo9 -c "only eo9:text $ hello --name boxed --excited true"
error: `only` refused: invalid allow-list: `eo9:text` is not an interface name (expected
`namespace:package/interface`)
failure(command-failed("error: `only` refused: invalid allow-list: …"))      (exit code 1)

$ eo9 -c "only eo9:text/text,eo9:text/types $ hello --name boxed --excited true"
error: `only` refused: the program still requires eo9:time/time@0.1.0, which the allow-list
does not include (allow it, compose a provider for it, or drop the requirement)
failure(command-failed("error: `only` refused: …same text again…"))          (exit code 1)

$ eo9 -c "only eo9:text/text,eo9:text/types,eo9:time/time,eo9:time/types $ hello --name boxed --excited true"
[1779929404.580859000] Hello, boxed!
ok: greeted
```

The coreutils: `ls` with no grant is refused with the same friendly message readwrite got;
with `--fs-root ./sandbox`, `ls --path .` lists `note.txt`, `cat --path note.txt` prints
`hisuccess(printed(2))` (the file has no trailing newline and the outcome line is glued
straight onto the program output), `echo --text "hi from eo9"` works, and an escape attempt
(`cat --path ../install-log.txt`) returns `failure(fs("FsError::Denied"))`, exit 1. Inside
the shell started with the same `--fs-root`, `ls --path .` shows `bin`, `session`, and
`note.txt` — two entries the participant never created.

Participant reaction: the clock refusal "is actually exactly what I was hoping it'd look
like. It never even starts, and the message names the specific missing thing … that's what
'decided before the program runs' meant." The escape denial was "satisfying … even if
`failure(fs("FsError::Denied"))` looks way more programmer-y than the nice readwrite message
— those two errors clearly weren't written by the same person on the same day." Gripes: this
is "the *second* README example that doesn't work as written … at this point I'd stop
trusting the README"; why must the no-authority `types` interfaces be listed in `only` at
all ("a lot of typing for the simple case"); and the `bin`/`session` entries — "are those
actual files sitting in my real sandbox folder now? … can a program I run inside the shell
read or trash the shell's own files? That feels like it pokes a hole in the clean sandbox
story." They asked to move on to writing their own program, but wanted expectations set
first: "do I *have* to write Rust … how many files … what's the step count … if the answer is
'learn Rust plus three new file formats plus five build commands,' I want to know that
upfront."

## Round 6 — overlay answers and authoring expectations

Verified live and shown: the host `sandbox/` directory still contains only `note.txt` — the
`bin`/`session` entries are layered virtually into the program's view from eo9's own data
directory; reading the shell's `session` file from inside (`cat --path session`) works and
prints a plain-English manifest of what the shell holds and what children receive; deleting a
shell program file (`rm --path bin/hello.wasm`) does **not** delete anything (verified on the
host), because the `/bin` layer is read-only — but the error it prints is
`error: fs("FsError::NotFound")` for a file `ls --path bin` lists right there.

Authoring expectations were given straight: Rust only (no Python/JS path exists), a "no
standard library" dialect, plus a small WIT interface file that doubles as the argument
parser and the permission list; 3 files plus 2 symlinks if the program lives inside the
repo's guest workspace; no `eo9 new`-style scaffold ("you copy the hello folder and edit");
then `cargo xtask build-guest`, one `eo9 store add`, and run. Doing it outside the repo is
rougher (no template, hand-wired SDK and WIT deps).

Participant reaction: the session manifest "is honestly the most readable thing this tool has
shown me all session — more of that, everywhere, please. Although 'children inherit the
shell's full environment' is a quiet little admission that the default is pretty generous."
The `rm` error "is exactly the kind of error that would send me on a 45-minute wild goose
chase … the real answer is 'that area is read-only.' The error should just say that."
"Rust, full stop" was "genuinely a bummer … I was quietly hoping there'd be a Python-ish path
since WebAssembly supposedly runs lots of languages," and "the no-standard-library kind"
"scares me more than the 30 lines do." The WIT-file idea "I actually kind of like
conceptually — one file that's both the argument parser and the permission list. That's the
pitch made concrete." They asked for hello's source first, then a new program: `repeat`,
taking `--word` and `--count`, printing the word that many times — chosen deliberately so its
imports should be text only, with `describe repeat` to prove it and a zero-flag run. "Leave
the mistakes in."

## Round 7 — writing `repeat`

Hello's three files were shown (Cargo.toml, `wit/world.wit`, `src/lib.rs`), with the honest
no_std cost for a program this size ("three boilerplate lines and `text::write_out_line`
instead of `println!`; where it bites is third-party crates"). The `repeat` crate was created
by copying hello's folder shape: a ~20-line WIT world (imports only `eo9:text/text@0.1.0`,
`main: func(word: string, count: u32)`), a ~30-line `lib.rs`, the same Cargo.toml with the
name changed, and one `wit/deps/text` symlink.

What actually happened, in order:

1. The Rust compiled on the first try — the compiler never produced an error all segment.
2. `cargo xtask build-guest` finished cleanly (~2s warm) **but produced no repeat
   component**, and nothing said so. `eo9 store add guest/target/components/eo9-example-repeat.wasm
   --name repeat` then failed: `eo9: error: i/o error on … No such file or directory (os
   error 2)` (exit 3).
3. Cause: `build-guest` componentizes a hand-maintained list of package names in
   `xtask/src/main.rs` (`GUEST_COMPONENTS`); a new crate under `guest/examples/` is built by
   cargo but silently never componentized. The fix was a one-line edit to the build tool's
   own source adding `"eo9-example-repeat"`, then `build-guest` again (3.1s) →
   `xtask: built component … eo9-example-repeat.wasm`.
4. `eo9 store add … --name repeat` printed the hash and binding; `describe repeat` showed
   args `--word: string`, `--count: u32` and imports of **text only** (no time, no fs);
   `eo9 repeat --word hi --count 3` ran from a non-repo directory with zero capability flags
   (`hi` ×3, `success(repeated(3))`); the strict
   `only eo9:text/text,eo9:text/types $ repeat --word strict --count 2` ran;
   `eo9 repeat --word oops --count banana` was refused before start with
   `bad arguments: argument `count` is not a valid `u32`: invalid value type at 0..6`
   (exit 3).

Participant reaction: "The Rust is less scary than I'd built it up to be. The body of `main`
reads basically like Python with semicolons and weird arrows … the top of the file is pure
voodoo … but it's copy-paste boilerplate. And the WIT file is honestly pretty readable …
I could write that one myself." The payoff "genuinely lands now … that's the pitch
demonstrated on a program *I* specified." But the build step: "yikes. That's the worst kind
of failure: the build says it finished cleanly, my program just silently isn't there, and the
fix is *editing the build tool's own source code* to add my program to a secret list. On my
own I would never, ever have found that … the silence is what kills it." Verdict on their own
bar: "split decision: the *concept* clears it … the *on-ramp* doesn't, not yet." Last
request: the actual `provider $ program` substitution — compose the frozen clock into hello.

## Round 8 — substitution: the trap, then the payoff

The obvious one-liner first, exactly as a newcomer would type it:

```
$ eo9 -c "time.frozen $ hello --name fixed --excited true"
abnormal: trapped: error while executing at wasm backtrace:
    0: 0xd79  - eo9_stub_time_frozen.wasm!__rustc[…]::rust_begin_unwind
    1: 0x59b8 - eo9_stub_time_frozen.wasm!core[…]::panicking::panic_fmt
    2: 0x68f1 - eo9_stub_time_frozen.wasm!core[…]::option::expect_failed
    3: 0xb59  - eo9_stub_time_frozen.wasm!eo9:time/time@0.1.0#now
    …
    7: 0xaff3 - eo9_example_hello.wasm!main: wasm trap: wasm `unreachable` instruction executed
failure(command-failed("abnormal: trapped: …whole backtrace again on one line…"))
(exit code 1)
```

The unconfigured frozen clock still panics at call time at this commit. The configured form
works and is deterministic (`time.frozen --now-seconds 0 --monotonic-ns 0 $ hello …` →
`[0.000000000] Hello, fixed!`, byte-identical on a second run), and the seeded-RNG version
was shown as the stronger demo: `eo9 rng --count 3` differs every run; `entropy.seeded
--seed 42 $ rng --count 3` produces the identical three numbers on every run.

Participant reaction: "that backtrace is exactly the kind of output that makes me close the
laptop … none of that tells me the actual problem, which turns out to be 'you forgot to tell
the frozen clock what time it is.' And the thing that stings more is that it breaks the rule
I'd *just* learned to trust … now I don't know which kind of failure to expect when I try
something new." But the working version "is the coolest thing you've shown me all day …
the idea that I could take a program I didn't write, not touch it at all, and from the
command line pin its clock to zero and its randomness to a seed, and it *can't tell* — that's
the first time the 'algebra' sentence from the pitch actually means something to me."
Final question: could they have discovered the provider names and flags themselves? Answer
shown in round 9: `eo9 store ls` lists all 37 names (programs and providers mixed), and
`describe time.frozen` / `describe entropy.seeded` do show `kind: provider` and their flags —
but nothing marks the flags as effectively mandatory, nothing connects the trap to the
missing flags, and `eo9 store --help` errors out instead of printing help.

## Wrap-up (the participant's structured answers, condensed, their words where quoted)

**Top 3 pain points**
1. The very first run failing despite following the README exactly — and the fix being "run a
   build command the README never mentions, then *reinstall the thing you already
   installed*", explained in an error message whose key words (store, bind, xtask, guest)
   they did not know.
2. The silent build registration for `repeat`: "success message + missing output +
   insider-knowledge fix is the worst combo."
3. The frozen-clock backtrace: "a wall of hex and 'rust_begin_unwind'" for what is actually
   a missing flag, breaking the refused-up-front-by-name rule they had just learned.
   (Honorable mentions, theirs: works-only-inside-the-repo, `ok:` vs `success(…)`, the `rm`
   "NotFound" lie, listing no-authority `types` interfaces in `only`.)

**Where they would have given up alone:** step 2 of the README — the "cannot find the eosh
component" screen two minutes after a successful install; at best they would have run
`build-guest` because the error suggested it, and then stopped when `eo9 hello` still failed.
The silent registration during `repeat` "would have been strike two and final."

**Words/concepts that needed explaining, and what made them click**
- "Imports are permissions" / capability — clicked at `describe hello` next to the readwrite
  denial; "the pitch sentence alone did nothing; the side-by-side demo did everything."
- "Decided before the program runs" — clicked at the `only` clock refusal; partially
  un-clicked at the frozen-clock trap.
- "Component" — inferred as "a .wasm program file with a declared interface"; never given a
  crisp definition.
- "Store" / "profile" — clicked from one plain sentence ("a folder where eo9 keeps programs
  it knows by name; a profile is a namespace inside it"); "the README has zero sentences."
- "Provider" / `$` — clicked at the seeded-RNG demo; "that demo is worth a thousand README
  sentences."
- Never clicked: "ambient authority", the bare-metal claim (not demonstrated), `&` layering,
  `rename`, `with … as slot`, "right-assoc", what WIT stands for.

**What a 10–15-minute beginner tutorial would need (their order):** one no-jargon paragraph
(phone-permissions analogy, then the difference); the *correct* install order tested on a
clean machine, every command copy-pasteable with expected output; `hello` then
`describe`/`env hello`; the readwrite denial and `--fs-root` grant with the real error text;
the seeded-RNG / frozen-clock swap *with the mandatory flags included*; "write your own" by
copying hello — including the registration step spelled out; and a short "when it goes wrong"
table mapping the five most common errors to what they actually mean. It must not open with
"ambient authority"/"object capabilities"/"algebra", must not show any example that was not
copy-pasted from a working terminal, must not silently assume the reader is standing in the
repo, and must not require reading a design document to learn what the store is.

**Would they try it again unprompted / recommend it to a peer?** "Honestly no" today, on both
counts — "they'd die at step 2 like I would have." Flips to yes if: (a) the README's two
commands work in order on a clean machine, (b) the build picks up (or at least loudly
mentions) new guest programs, (c) provider misconfiguration is refused up front like
everything else instead of a backtrace. "Those three and I'd genuinely give it a weekend
afternoon — the Rust-only thing wouldn't stop me at the copy-and-edit level, though a Python
or JS path would make it way easier to drag friends in."

**Genuinely impressed:** the seeded-RNG / frozen-clock substitution on an unmodified program
("the thing I'd actually tell a friend about tomorrow"); the readwrite refusal message
("make every error like that"); the `cat --path session` plain-English manifest; `describe`
working uniformly on programs and providers; the sandbox escape simply being denied with the
real disk untouched; and `repeat` running with zero flags because it asked for nothing
dangerous — "that's the pitch, working, on something I specified. The idea is good. The
on-ramp is what's broken."

## Findings

### Bugs / rough edges verified during the session

1. **The README's getting-started order does not work from a fresh checkout.**
   `cargo install --path crates/eo9` before `cargo xtask build-guest` produces a binary with
   an empty embedded component set; the first `eo9` run then fails with "cannot find the eosh
   component" (exit 3) instead of seeding and dropping into eosh, and `eo9 hello …` fails
   with `name hello does not resolve in profile "default"`. The README's userspace section
   never mentions `build-guest`. Recovery requires build-guest **plus a reinstall** (or
   manual `store add`); build-guest alone only fixes the shell, and only from the checkout
   root.
2. **Behavior depends on the current directory.** With components built but not embedded,
   `eo9` works from the checkout root (dev-tree fallback on `guest/target/components/`
   relative to cwd) and fails everywhere else; direct `eo9 hello` fails everywhere until the
   store is populated. The participant called this "the kind of invisible behavior that makes
   me think I broke something."
3. **The eosh-missing error message is jargon-dense for the audience that hits it** (store,
   bind, xtask, guest, development tree), and the README never defines "store" or "profile"
   (the word "store" appears once, in a comment).
4. **The README's `only eo9:text` package-level shorthand still does not parse** (the
   refusal text is now a readable sentence rather than a raw enum debug print — an
   improvement since study 01 — but the example remains broken as written, and the README's
   shown error text differs from what actually prints).
5. **Shell-path refusals print their message twice and exit 1.** `only` refusals through
   `eo9 -c` print an `error:` line and then the same text wrapped in
   `failure(command-failed("…"))`, exiting 1, while the equivalent direct-run refusal exits
   3 — two different exit codes for "refused before start" depending on the front door.
6. **No-authority `types` interfaces must still be listed in `only` allow-lists**, so the
   minimal "text and clock only" command is four full `namespace:package/interface@version`
   names long.
7. **The outcome line is glued onto program output**: `cat` of a file with no trailing
   newline prints `hisuccess(printed(2))` (and `hiok: printed(2)` in the shell); outcome
   rendering also still differs between front doors (`success(greeted)` vs `ok: greeted`).
8. **Error-quality is inconsistent across denial paths**: pre-start refusals are excellent
   (readwrite/ls), but the in-sandbox escape attempt surfaces as
   `failure(fs("FsError::Denied"))` (internal enum text, exit 1), and deleting a file on the
   read-only `/bin` overlay layer reports `fs("FsError::NotFound")` for a file `ls` lists —
   the protection holds but the explanation is wrong-sounding.
9. **The shell's session overlay surprises newcomers**: `ls --path .` inside a `--fs-root`
   shell session shows `bin` and `session` entries the user never created. (Verified good:
   nothing is written into the user's real directory, the `/bin` layer is read-only, and the
   `session` manifest is readable and clearly written.)
10. **Adding a new guest program is silently ignored by the build.** `cargo xtask build-guest`
    componentizes only the hand-maintained `GUEST_COMPONENTS` list in `xtask/src/main.rs`; a
    new crate under `guest/examples/` builds but never becomes a component, the xtask exits
    successfully with no warning, and the failure only appears later as an unrelated-looking
    `store add` "No such file or directory". The fix (editing the build tool's source) is not
    discoverable.
11. **The unconfigured `time.frozen $ hello` one-liner still traps at runtime** with a raw
    wasm backtrace (and the whole backtrace duplicated inside `failure(command-failed(…))`),
    exit 1 — the known compose-time-configuration gap, hit here by the most obvious command a
    newcomer would type for the headline feature. `describe time.frozen` does show the
    provider's flags, but nothing marks them as required and nothing links the trap back to
    them.
12. **`eo9 store --help` errors** ("unknown store action `--help`: expected add, ls, or gc")
    instead of printing help.

### Confusions observed (novice-specific)

- The pitch vocabulary itself: "Component Model", "capability", "ambient authority",
  "algebra", "component" (circular on first contact), "provider", "slot", "right-assoc",
  WIT, no_std. The phrases only acquired meaning through demos (describe + denial for
  imports-are-permissions; seeded RNG for provider/compose), never through the prose.
- The recovery vocabulary: "store", "bind", "profile", "xtask", "guest", "development tree" —
  all first encountered inside an error message on the very first run.
- The "deny by default" claim vs. the silent default grant of text/time/entropy on direct
  runs ("deny by default, except for the stuff we decided is fine").
- Reinstalling to change behavior ("reinstalling to me means 'do the same thing again'"), and
  behavior changing with the current directory.
- Which failure shape to expect: refused-before-start with the missing thing named (most
  paths) vs. a mid-run trap with a backtrace (misconfigured provider) vs. a typed
  `failure(…)` line (runtime fs denial).

### What landed well

- The `readwrite`/`ls` missing-filesystem refusal message (named the capability, the reason,
  and the exact flag; exit 3 before anything ran) — the participant's standard for every
  other error.
- `describe` and `env` on programs *and* providers, including provider flags; `store ls`.
- The `cat --path session` manifest — the most readable surface of the session.
- The seeded-RNG / frozen-clock substitution demo (once configured) — the moment the pitch
  landed; the wrong-type argument refusal naming the argument; `repeat` running with zero
  flags because it imports only text.
- The sandbox story under test: escape attempt denied, host directory untouched, shell `/bin`
  layer effectively read-only.
- Build/install mechanics: 1m17s cold install, ~10s re-install, ~40s full guest build, ~3s
  incremental; the `only` and `invalid allow-list` refusals now reading as sentences rather
  than enum debug prints (improvement since study 01).
- The participant's own program compiled first try; WIT was judged "honestly pretty readable"
  and the no_std cost negligible at this size.

### Feature requests / asks from the participant

- Make the README's two-command getting-started true in the order written (or change the
  documented order), tested on a clean machine; define "store" in one sentence; show real
  error text.
- A loud warning (or automatic pickup) when a guest crate exists but is not in the
  build-guest component list; longer-term, an `eo9 new`-style scaffold and a registration
  step that is part of the documented authoring flow.
- Refuse misconfigured providers up front, by name, like every other missing thing — or at
  minimum translate the trap into "time.frozen was composed without --now-seconds".
- One error-rendering pass: same friendliness as the readwrite message for the eosh-missing
  error, the runtime fs denial, and the read-only-layer delete (say "read-only", not
  "NotFound"); stop printing refusal text twice; align exit codes between front doors.
- Let `only` accept a shorter spelling and stop requiring the no-authority `types`
  interfaces.
- Keep the outcome line off the program's output line (newline handling at minimum), and pick
  one rendering (`ok:` vs `success(…)`).
- A beginner tutorial with the structure and prohibitions listed in the wrap-up; a plain-
  English glossary (component, provider, store, profile, capability) linked from the README.
- A statement on non-Rust guest languages (even if the answer is "not yet").
- `eo9 store --help` should print help.

## Facilitator observations

- The two biggest findings of the session (the broken install order and the silent
  build-guest registration list) were not planned demonstrations — the facilitator hit both
  while following the README and the documented authoring shape, and showed them as they
  happened.
- The participant's prediction in round 1 ("does it spit out a wall of warnings, does it even
  work?") was directed at the wrong step: the Rust toolchain parts (install, compile,
  first-try guest build) were smooth throughout; every failure came from project wiring
  (embedding order, cwd-dependent fallbacks, hand-maintained build lists) or error rendering.
- Improvements relative to study 01 were visible at this commit: `only` refusals and
  invalid-allow-list errors are human-readable sentences, the session manifest/`env` output
  is plain English and states what children inherit, and provider flags are discoverable via
  `describe`. The unconfigured-provider trap and the package-level `only` shorthand remain as
  they were.
- Study-environment deviations from a literal first-day experience, all disclosed in the
  transcript: install `--root` redirected to a study prefix, `EO9_STORE` pointed at a
  throwaway directory, shell sessions driven over piped stdin, and a host that already had
  the pinned nightly toolchain, wasm32 target, and `wasm-tools` — so toolchain-bootstrap
  friction for a genuinely clean laptop is untested by this session.
- The `repeat` crate (3 files + 1 symlink), the one-line `GUEST_COMPONENTS` addition in
  `xtask/src/main.rs`, and the study store/install prefix were left as uncommitted session
  artifacts in the worktree; only this report is committed on the study branch.
