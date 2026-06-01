# User study 10 — returning novice (saw the round-1 site, came back to see if they get it now)

## Session metadata

- **Date:** 2026-05-31
- **Branch / worktree:** `docs/study-10` (worktree of master at `5985249`)
- **Participant persona:** a curious developer, ~6 years in — TypeScript/React frontends, Node and
  Python services, Docker, a little Go. Comfortable in a terminal, NOT a systems programmer: never
  written Rust, never used QEMU, knows WebAssembly only as "that thing that makes some browser apps
  fast", never heard of the Component Model or capability security as terms. They visited eo9.org
  once, months ago (the round-1 site), bounced off it in two minutes, and remember only "a wall of
  unfamiliar jargon, nothing you could actually try" and the half-phrase "capability something".
  They are returning to see whether they get it now. Zero other Eo9 context, no repository access.
- **Session focus:** the returning-visitor experience — does the site as it stands today land for
  someone the round-1 site filtered out? What clicks, what is still confusing, what is newly
  confusing? Compared throughout against the round-2 novice study (`docs/user-studies/06-novice.md`).
- **Methodology:** the participant was a role-played persona run as a separate session with no
  access to the repository or any tools — it saw only what the demoer pasted in, in stages, and
  replied conversationally. Every page shown was actually served and fetched; every command shown
  was actually executed; outputs are verbatim, trimmed only for length. When the participant asked
  to type something the demoer had not yet run, the demoer ran it for real and showed the result —
  including the participant's deliberate breaking attempts and their mistakes, uncorrected.
- **Environment:** Apple Silicon macOS host. The website was built (`cargo build` in `www/`, 8.2 s)
  and served with the real server (`eo9-www --site site --bind 127.0.0.1:8097`); both pages were
  fetched over HTTP. The browser-terminal sessions were produced with the committed `/vm` blob
  driven through Node v25 (JSPI) using the same import glue as `www/site/vm/vm.js` — first
  `node www/web-eo9/verify-eosh.mjs` (all 28 checks pass at this commit), then a transcript harness
  feeding the participant's exact keystrokes through the page's `read-line` path. Usermode demos
  used a fresh `eo9` build (`cargo build -p eo9` + `cargo xtask build-guest`, then a rebuild so the
  binary embeds the just-built components, per the README's documented order) with `EO9_STORE`
  pointed at an empty throwaway directory, so every "first run" shown is what a brand-new machine
  sees. Disclosed deviations: the store redirection above; interactive usermode segments were driven
  over piped stdin (typed commands are shown where they were typed); browser-terminal line spacing
  is presented compacted (see facilitator observations).
- **Shape:** six participant phases — front page → try-it page → their first terminal session →
  their follow-up session (sealing, `let`/`save`) → their predictions + the laptop story →
  structured exit interview.

## The website walkthrough (what was served and captured)

The site server built in 8.2 s and serves the committed site:

```
$ cd www && cargo build
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 8.18s
$ ./target/debug/eo9-www --site site --bind 127.0.0.1:8097

$ curl -s -o /dev/null -w "%{http_code} %{size_download}\n" http://127.0.0.1:8097/
200 7901
$ curl -s -o /dev/null -w "%{http_code} %{size_download}\n" http://127.0.0.1:8097/vm/
200 5829
$ curl -s -o /dev/null -w "%{http_code} %{size_download}\n" -H "Accept-Encoding: br" \
    http://127.0.0.1:8097/vm/web-eo9.4e9e89baa375be13.wasm
200 1742167
```

The front page (`/`) is one screen of prose: hero ("A capability-secure operating system built on
the WebAssembly Component Model"), three links (Try it in your browser / Source on GitHub / Read
the spec), an early-development status note, then sections: What Eo9 is / Composition is the whole
interface (with a three-line shell example) / Virtualization and sandboxing by substitution /
Deterministic execution / Performance / eofs / Bare metal and hosted / Where it stands.

The try-it page (`/vm/`) is the terminal (`fetching the Eo9 OS (about 1.8 MB compressed)…` — the
brotli-compressed blob measured above is 1.74 MB, so the claim is honest), an "Explore the sandbox"
section teaching the loop (`help` → `ls /bin` → `describe hello` / `describe entropy.seeded` →
`entropy.seeded --seed 7 $ rng --count 2`), a "Things to try" list (bare `hello`, seeded rng, two
`only` lockdowns, the frozen clock, coreutils against the in-memory fs), and a "What this is
actually doing" section that names the one honest difference (Pulley interpreter instead of native
codegen) and the requirements (JSPI-capable browser).

## Phase 1 — the front page

The participant was shown the front page text and asked to compare against what they remember
bouncing off.

Their reaction, condensed (their words quoted): "this is genuinely not the page I remember…
First thing my eyes hit: '[Try it in your browser]'. That alone is a different website than the one
I bounced off. Last time my impression was 'wall of jargon, nothing to touch.' Now there's a
button."

**What clicked, and what made it click:**

- "The import set therefore IS the capability set" — "I read that three times and then went '…oh.
  It's dependency injection.' Like, a module that can't `import fs` on its own — it can only use
  what you hand it. I do this every day in tests when I mock `fetch`… here it's not a convention
  you can cheat around, it's just physically how the programs work."
- `net.none $ browser # browser observes "no network"` — "That's the whole pitch in one line… I
  would 100% use that for 'does this sketchy npm package phone home' type questions."
- Typed args/outcomes ("no untyped argv… typed vocabulary rather than a numeric exit code") — "the
  TypeScript developer in me felt that."
- Deterministic execution — "I fight flaky tests for a living… If that's real, that's the feature
  I'd tell a friend about, weirdly, more than the security stuff."
- The honesty: the status box, "One honest caveat", the Where-it-stands bullets — "a project page
  that volunteers a performance downside earns a lot of trust from me."

**What still loses them (their quotes, exact):**

- "built on language-theoretic principles" — "this means nothing to me… It's literally the second
  sentence of 'WHAT EO9 IS'." (This is where they report past-them bounced.)
- "attenuating" — "I know that word from like, audio cables."
- "sealing" — "Sealed how, against what?"
- "fuel-metered" — "Gas like Ethereum? CPU time? No idea."
- "MMU-less… linear-memory safety… bounds checks the optimizer cannot prove away" — "I understood
  maybe 30% of it… I'm not the audience for this sentence and that's probably fine."
- "the compiler, the root scheduler, and the hardware-root capabilities held by the OS core" — "the
  parenthetical is supposed to be reassuring, I think, but it's three more terms I don't know."

**Actively bugged them:** the operator choice. "Twenty years of every tutorial on earth using `$`
as the shell prompt, and they made it the compose operator… Same with `&` — in my world `&` means
'run in background'… it made me stumble on every example."

**The question the page never answers:** "what do I actually *do* on this OS?… programs have to be
'WebAssembly Component Model components,' so… nothing I have runs on it, right?… [in] what
language… That's the gap between 'this is a cool idea' and 'this is a thing I could use,' and the
page kind of skips over it."

**Next action:** "click 'Try it in your browser,' no contest… I would *not* click 'Read the spec'."

## Phase 2 — the try-it page

The participant was shown the try-it page as it loads, through to the live `eosh>` prompt.

First reaction: "**it booted in about a second and the whole OS is 1.8 MB.** I have shipped *React
apps* bigger than this operating system."

**What read well:** the explore loop framing ("They're handing me a workflow, not a feature list"),
`describe` as a concept ("it's `--help` except… derived from the actual types. If that's real,
that's better than 90% of CLI tools I use"), the verifiable phrasing of Things to try ("'Run it
twice — identical output'… means I can verify their claims instead of just looking at output and
going 'huh, neat I guess'"), and the honesty section again ("They keep doing this and it keeps
working on me").

**What did not:** the boot banner. "'the pinned wasmtime (45.0.0, Eo9's vendored copy) compiled to
wasm32, Pulley interpreter, fiberless component-model-async'… 'pre-AOT'd to pulley32'… that's debug
output… it's the first text the terminal shows a newcomer and it's the least newcomer-friendly text
on the page."

**Sharp catch:** "the front page wrote `only eo9:text,eo9:time $ hello` and this page writes
`only eo9:text/text,eo9:time/time $ hello`. Which is it?… Somebody's going to copy the front-page
version and have it not work. (Or maybe both work? I'll find out, I guess.)"

They then listed the commands they wanted to type — the page's loop in order, then their own run of
`hello` with their name, the determinism double-run, the `only` refusal — "and fair warning, after
those I'm going to try to break something."

## Phase 3 — their first terminal session

Every command below was run for real in the page's blob through the page's read-line path; the
participant saw this transcript. Key excerpts (`[stderr]` prefixes are verbatim — that is what the
page terminal renders):

```
eosh> describe hello
kind: binary
args:
  --name: option<string>
  --excited: option<bool>
imports:
  required eo9:text/types (eo9:text/types@0.1.0)
  required eo9:text/text (eo9:text/text@0.1.0)
  required eo9:time/types (eo9:time/types@0.1.0)
  required eo9:time/time (eo9:time/time@0.1.0)
  required eo9:rt/diagnostics (eo9:rt/diagnostics@0.1.0)
…
eosh> hello --name sam --excited true
[1780288332.477000000] Hello, sam!
ok: greeted

eosh> entropy.seeded --seed 43 $ rng --count 3        (run twice — identical numbers both times)
13432527470776545160
11303639812522640203
7982107704362031207
ok: generated(3)

eosh> only eo9:text/text $ hello
[stderr] error: `only` refused: the program still requires eo9:time/time@0.1.0, which the allow-list does not include (allow it, compose a provider for it, or drop the requirement)
[stderr]

eosh> only eo9:text,eo9:time $ hello                  (the front page's short form — works)
[1780288332.724000000] Hello, world.
ok: greeted

eosh> hello --name sam --excited yes                  (their breaking attempts begin here)
[stderr] error: bad arguments: `yes` is not a bool
[stderr]

eosh> hello --nmae sam
[stderr] error: unknown flag `--nmae`: the program declares no such parameter
[stderr]

eosh> frobnicate --level 11
[stderr] error: cannot resolve `frobnicate` (/bin/frobnicate.wasm): FsError::NotFound
[stderr]

eosh> cat /nope.txt
error: fs("FsError::NotFound")
```

**The moment the system became reasoned-about rather than demoed** (their words): "1. `describe
hello` says it imports `eo9:time/time` — 'why does a hello-world need *the time*?' 2. Then I look
at the output: it prints a timestamp. *That's* why. 3. And that's exactly why `only eo9:text/text $
hello` got refused… I could have *predicted* that failure from the `describe` output… That's the
moment this stopped being a demo and started being a system I could reason about."

**On the `only` refusal:** "genuinely the best CLI error I've seen in a while… It tells me what's
missing, AND gives me three ways to fix it, AND — this is the part that matters — nothing ran."

**On the breaking attempts:** the typed-argument errors were praised ("'`yes` is not a bool' —
immediate, obvious"; "'the program declares no such parameter' — it's checking against actual
declared parameters"). But: "the filesystem errors are from a different, much worse universe…
`FsError::NotFound` — that double-colon is somebody's internal enum leaking straight through to me.
And `fs("FsError::NotFound")` is even worse, that's just raw debug formatting… it's the only place
in this whole session where the polish cracked."

**A catch nobody planned:** "describe hello says it requires `eo9:rt/diagnostics`. But
`only eo9:text,eo9:time $ hello` — an allow-list that does NOT include diagnostics — ran fine… So
is `eo9:rt/*` exempt from `only`?… a category of import that bypasses the allow-list is exactly the
thing I'd want explained." (Demoer's answer — diagnostics is the no-authority crash-reporting
interface, exempt by design — satisfied them, but "Nothing on the page or in the shell explains
this — you had to ask me.")

Also flagged: "a parenthesized argument is passed **open**, not run" in `help` — "no idea what
'open' means there."

## Phase 4 — sealing, `let`/`save`, and the broken help example

The participant designed their own tests for "sealing" and "reusable environments are ordinary
values". All run for real:

```
eosh> imports rng
imports:
  required eo9:entropy/types (eo9:entropy/types@0.1.0)
  required eo9:entropy/entropy (eo9:entropy/entropy@0.1.0)
  required eo9:text/types (eo9:text/types@0.1.0)
  required eo9:text/text (eo9:text/text@0.1.0)
  required eo9:rt/diagnostics (eo9:rt/diagnostics@0.1.0)

eosh> imports (entropy.seeded --seed 43 $ rng)
imports:
  required eo9:rt/diagnostics (eo9:rt/diagnostics@0.1.0)
  required eo9:text/types (eo9:text/types@0.1.0)
  required eo9:text/text (eo9:text/text@0.1.0)
```

"The entropy import doesn't get 'satisfied' in some bookkeeping sense — it's *removed from the
list*… THAT'S what 'satisfying and sealing the imports it matches' meant on the front page. Three
commands and a word I flagged as jargon two stages ago now has a concrete meaning in my head. This
is how you teach people things."

Then the worst thing in the session. The participant typed the `&` example **from the shell's own
help text** (`e.g. time.frozen --now-seconds 0 & entropy.seeded --seed 7`):

```
eosh> let det = time.frozen --now-seconds 0 & entropy.seeded --seed 7
[stderr] error: missing argument `--monotonic-ns` (a u64)
[stderr]
eosh> det $ rng --count 2
[stderr] error: cannot resolve `det` (/bin/det.wasm): FsError::NotFound
[stderr]
```

The demoer confirmed the help example fails identically on its own (it is not a `let` problem:
`time.frozen` refuses partial configuration). The participant: "**The example in the official help
text fails.** And if you, the facilitator, hadn't told me… I would have assumed *I* screwed up —
that's what newcomers do, we blame ourselves… Honestly it's a little ironic: the system whose whole
pitch is 'deterministic tests go all the way down' has untested examples in its help text." They
also flagged the cascade error: "`det` was never supposed to be a file; it was supposed to be a
`let` binding. 'no such binding or program `det`' would have pointed me at the actual problem."

The corrected forms then worked, and worked well:

```
eosh> let det = time.frozen --now-seconds 0 --monotonic-ns 0 & entropy.seeded --seed 7
eosh> det $ rng --count 2          (twice — identical)
7191089600892374487
309689372594955804
ok: generated(2)
eosh> det $ hello
[0.000000000] Hello, world.
ok: greeted
```

"One named environment, applied to two different programs, everything deterministic. That's a test
fixture… I can already feel where this goes — 'run my whole integration suite under `det`' — and I
want it." (Note: `let` succeeding prints nothing — you cannot tell it worked until you use it.)

`save` worked but exposed a discrepancy:

```
eosh> save greet = time.frozen --now-seconds 0 --monotonic-ns 0 $ hello
saved: /bin/greet.wasm (run it as `greet`)
eosh> ls /bin
cat.wasm … time.frozen.wasm           (7 files — greet.wasm is NOT listed)
ok: listed(7)
eosh> describe greet                   (works)
eosh> greet                            (works: [0.000000000] Hello, world.)
```

"So… it's in /bin and also not in /bin?" The participant correctly theorized the cause from `env`'s
own text (the shell's store vs the fresh per-run filesystem programs see) and concluded: "If that's
right, it's technically consistent and *completely* confusing — 'is the file there?' should not
depend on who's asking."

Last: `/welcome.txt` itself says "Try: cat /welcome.txt, ls /, wc /welcome.txt." — and `wc` does
not exist in the browser's 7-program store:

```
eosh> wc /welcome.txt
[stderr] error: cannot resolve `wc` (/bin/wc.wasm): FsError::NotFound
```

"That's **two** examples, written by the project, inside the product, that fail when you type
them."

Also on the record from this phase: "quietly, `env` is the best output in the whole system…
Whoever wrote `env` should rewrite the boot banner."

## Phase 5 — predictions confirmed, and the laptop story

The participant made a prediction: `greet` (hello with the clock sealed in) should pass the
allow-list that refuses `hello`. Run for real, back to back:

```
eosh> only eo9:text $ hello
[stderr] error: `only` refused: the program still requires eo9:time/time@0.1.0, …
eosh> only eo9:text $ greet
[0.000000000] Hello, world.
ok: greeted
eosh> greet --name sam --excited true
[0.000000000] Hello, sam!
ok: greeted
eosh> describe det                     (a let binding can be described: kind: provider,
                                        exports entropy + time + rt/configured)
```

"I took a program, sealed its clock dependency inside it, and the result is *less privileged* than
the original… And 'attenuating' — the word I mocked in stage 1 as audio-cable vocabulary — that's
just… this. Making a program need less. Every piece of jargon on that front page has now turned
into something I personally typed and verified."

The laptop story (all real output; `make help`, the seeded first run, direct runs, the `&` mistake):

```
$ eo9
eo9: first run: seeded 50 bundled programs into the module store at /tmp/study10-store
eosh — the Eo9 shell (type `help` to explore, `ls /bin` to see what's installed)
eosh> hello
[1780287935.062077000] Hello, world.
ok: greeted
eosh> entropy.seeded & echo
error: `&` refused: `echo` is a program, not a provider — `&` combines providers into an
environment; to run it with that provider use `entropy.seeded $ echo`

$ eo9 hello
[1780287935.572709000] Hello, world.
success(greeted)

$ eo9 cat notes.txt
eo9: error: cat (store object a95fe…6bd3) requires the eo9:fs filesystem capability, which eo9
does not grant by default: pass `--fs-root <dir>` to give the program access to a host directory
(guest paths cannot escape that root)

$ eo9 -c 'entropy.seeded & echo'
error: `&` refused: `echo` is a program, not a provider — … use `entropy.seeded $ echo`
$ eo9 -c 'entropy.seeded & echo --text hi'
error: arguments cannot be applied to an `&` operand
```

Reactions: the install story is "a non-event, in a good way… It's not Ubuntu. It's closer to… if
Docker and a test framework had a weird baby." ("pinned nightly" Rust made them "slightly
nervous".) The fs refusal is "**the single best thing I've seen in five stages**… Meanwhile, every
npm postinstall script I've ever run could read `~/.ssh` and nobody asked me anything. This is the
thing I'd tell a friend… That sentence sells itself."

But the polish pattern got named: "Same mistake, two different errors… `entropy.seeded & echo` gets
a *beautiful* error… but add `--text hi` and I get 'arguments cannot be applied to an `&` operand',
which is a shrug in error-message form." And: "`ok: greeted` in the shell vs `success(greeted)` in
direct-run mode… The day I write a script that parses this output, that inconsistency costs me an
hour." Their summary: "the *designed* surfaces (only, describe, env, the fs refusal) are
exceptional, and the *edges between* surfaces (shell vs CLI, fs errors, help examples) are
unfinished."

The Rust-only answer: "honest, and it's the answer I feared… But — two things soften it. First,
'copy the hello example and edit it, ~30 lines' is how I have learned literally every language I
know. Second, the detail that the WIT file is **where `describe`'s output comes from**… The typed
interface I've been admiring all session isn't generated magic, it's a file I'd write." Their
follow-up question — "is Rust-only a *today* thing or a *forever* thing?" — has a good answer (the
format is language-neutral by design) that appears nowhere on the site.

Verdict on next weekend: "Yeah. Honestly, yes… What I'm *not* doing is using this for anything real
— there's no networking yet, no language I work in… This is a toy. But it's the best-explained toy
I've picked up in years, and six months ago this same project bounced me off its front page in two
minutes. So to answer the question I walked in with: it wasn't me. They made it make sense."

## Phase 6 — exit interview (condensed, their structure)

**Top 3 pain points**
1. **The product's own examples don't run** (the help `&` example, the welcome-file `wc`). Would
   not have stopped them alone ("the error message names the missing argument") but "it's the
   moment where I'd have stopped trusting everything else I read."
2. **The most important adoption question isn't on the site**: what do I write programs in, and is
   Rust-only forever? "Both answers are good! Neither is written anywhere… it absolutely determines
   whether I invest a weekend."
3. **The small unexplained inconsistencies** (diagnostics through `only`, save vs `ls /bin`,
   `ok:` vs `success(…)`): "Each one is a 'is this broken or am I dumb?' moment… The system is
   consistent; the *explanation* is missing."

**Where they would have given up alone:** "Today: nowhere, and that genuinely surprises me. But the
margin was thin and it was exactly one button wide" — they only survived the front page's
"language-theoretic principles" sentence because Try-it-in-your-browser was visible above the fold.
"The real untested give-up point is next weekend: if `make setup` fights me on my Mac, that's where
this ends, and no website can save it."

**The returning-visitor question — what changed the outcome:** "not me… They changed": (1) there's
something to try — "this is 70% of the answer by itself"; (2) one sentence connects to their
existing knowledge (imports = dependency injection); (3) claims are phrased as checkable
predictions; (4) volunteered limitations. "One big thing (the demo) that only worked because of
many small things… Better prose alone would not have kept me."

**Vocabulary scorecard.** Clicked, with the exact moment: capability (describe + the refusal),
sealing (the imports before/after), attenuating (greet passing the allow-list that refused hello),
provider (`time.frozen.wasm` sitting next to `hello.wasm` in /bin), composition (using `$`/`&`/
`let`), deterministic execution (verified twice), WIT ("the file `describe` reads from. Best new
word of the day"). Still meaningless: fuel-metered, language-theoretic principles ("I now suspect
it doesn't need to exist"), fiberless component-model-async, linear-memory/MMU sentence,
hardware-root capabilities, "passed open, not run".

**Trust scorecard.** Eight claims checked (three as predictions made before seeing output), zero
failed. "What failed was never a claim about the system — it was documentation hygiene around the
system… I now extend provisional trust to bare metal and 'deterministic all the way down,' because
this project has a 100% hit rate on falsifiable statements."

**Recommend to a peer?** Yes — to the backend friend with the flaky integration suite (the
determinism story) and the friend who writes Rust. Not to frontend colleagues ("there's nothing for
them to make yet"). Recommended first contact: skip the front page, go to /vm, type `describe
hello`, `hello --name you`, `only eo9:text/text $ hello`, `time.frozen --now-seconds 0
--monotonic-ns 0 $ hello`. "Four commands, two minutes, you'll get it."

**The one fix:** "Run every example you ship — in `help`, in `/welcome.txt`, on the website — in
CI, and fail the build if one doesn't work… this project, of all projects, has no excuse. Your
front page brags about 'law tests and a generative property suite'… Point the machinery at your own
docs. It's an afternoon of work and it protects everything else."

**On the record:** (1) "The project's best writer is currently assigned to its least visible
surface" (`env` vs the boot banner / front-page opening / fs errors). (2) The meta-finding:
"Every question I asked, you had a good answer for… The project isn't missing knowledge; it's
missing the transcription of answers that already exist in someone's head. 'You had to ask me' came
up four times today. The next person doesn't get to ask." (3) "Six months ago this project's front
page filtered me out in two minutes, and today I'm planning my Saturday around it… The difference
is they let me check their claims instead of asking me to believe them."

## Comparison with the round-2 novice (study 06)

| Round-2 complaint (06) | Status in this session |
|---|---|
| #1 README install order broken; first run fails with "cannot find the eosh component" | **Fixed and verified.** README now puts `build-guest` first; `make shell` does it in one step; a fresh empty store seeds 50 programs with a clear message (`eo9: first run: seeded 50 bundled programs into the module store at …`) and drops into the shell; the binary also falls back to a prebuilt component bundle if built in the wrong order. |
| #2 Behavior depends on cwd | **Not reproduced.** First run from a non-repo directory worked (store-seeded path). Not exhaustively retested. |
| #3 eosh-missing error jargon (store, bind, xtask, guest) | **Moot** — the error no longer occurs on the happy path. |
| #4 `only eo9:text` package shorthand doesn't parse | **Fixed and verified** (browser and usermode; both the page's long form and the front page's short form run). |
| #5 Shell refusals print twice / exit codes differ between front doors | **Partially.** No double print observed in the browser path. `ok:` vs `success(…)` and exit-code unification still open (tracked, R2-18 / synthesis #4). |
| #6 No-authority `/types` interfaces must be listed in `only` | **Fixed and verified** — `only eo9:text/text,eo9:time/time $ hello` and `only eo9:text,eo9:time $ hello` both run. |
| #7 Outcome line glued to program output; `ok:` vs `success(…)` | **Partially** — gluing not observed this session; the two renderings remain (participant: "costs me an hour" when scripting). |
| #8 fs errors leak internal enum text (`FsError::…`) | **Not fixed** — hit four times in the browser (`frobnicate`, `cat /nope.txt`, the `det` cascade, `wc`). Tracked (GAPS error-quality consistency). |
| #9 Session overlay surprises (`bin`/`session` entries) | **Improved** — `env` now explains the layout in plain English; but a new variant appeared (save/`ls /bin` disagreement, finding 4). |
| #10 New guest crate silently not componentized | **Not retested** (authoring out of scope this round). Tracked. |
| #11 Unconfigured `time.frozen $ hello` traps with a wasm backtrace | **Fixed and verified** — runs with documented defaults (`[946684800.000000000] Hello, world.`). But a new adjacent trap appeared: *partial* configuration is refused, and the help's own example is partial (finding 1). |
| #12 `eo9 store --help` errors instead of printing help | **Not fixed** (now lists `reseed` as a fourth action). Tracked. |
| Round-2 wrap-up: "would not try again / not recommend" | **Flipped.** This participant ends planning a weekend install and naming two specific people to recommend it to. The round-2 novice's three flip conditions (working install order, loud build pickup, no provider traps) are 2-for-3 done; the remaining one (build pickup) was not exercised here. |

**Newly confusing this round (not present or not reachable in round 2):** the boot banner jargon;
`eo9:rt/diagnostics` appearing in every `describe` and being silently exempt from `only`; the
save/`ls /bin` disagreement; silent `let`; the `&` teaching error vanishing when arguments are
present; "passed open, not run"; the front-page vs try-it-page interface-name spelling difference.

## Findings

### Bugs / rough edges verified during the session

1. **The shell help's own `&` example fails.** `help` prints "e.g. `time.frozen --now-seconds 0 &
   entropy.seeded --seed 7`"; typing exactly that yields ``error: missing argument `--monotonic-ns`
   (a u64)`` because `time.frozen` refuses partial configuration (all-or-nothing). Hit by the
   participant on their first attempt to use `&`.
2. **`/welcome.txt` recommends a program the browser store doesn't have.** The file says "Try: …
   `wc /welcome.txt`"; the page's `/bin` has 7 programs and `wc` is not one of them →
   ``error: cannot resolve `wc` (/bin/wc.wasm): FsError::NotFound``.
3. **The `&` operand teaching error disappears when arguments are present.**
   `entropy.seeded & echo` → the excellent error naming the problem and the exact fix;
   `entropy.seeded & echo --text hi` → `error: arguments cannot be applied to an `&` operand`
   (terse, no fix). The argument check fires before the program-vs-provider check, so the most
   natural form of the mistake gets the worst message. Same in shell and `-c`.
4. **`save` and `ls /bin` disagree in the browser.** `save greet = …` reports
   `saved: /bin/greet.wasm (run it as `greet`)`; `greet` runs and `describe greet` works; but
   `ls /bin` still lists 7 files. Cause: the shell's store vs the fresh per-run fs that programs
   see. Technically consistent, observably contradictory.
5. **Failed-`let` cascade error points away from the cause.** After a failed `let det = …`,
   `det $ rng` → ``error: cannot resolve `det` (/bin/det.wasm): FsError::NotFound`` — the message
   never mentions that `det` could have been a binding, and leaks the fs enum.
6. **`let` succeeds silently.** No confirmation output; the user cannot tell the binding exists
   until they use it (or `describe` it).
7. **`eo9:rt/diagnostics` is visible everywhere and explained nowhere.** It appears as `required`
   in every `describe`, yet `only` allow-lists that omit it still pass. The design is sound
   (no-authority crash reporting); no surface says so.
8. **fs-flavored errors still leak internal enum text** (round-2 #8, still open): `FsError::NotFound`
   in program-resolution errors and `error: fs("FsError::NotFound")` for missing files, vs the
   polished argument/`only`/fs-capability refusals.
9. **The browser boot banner is the least newcomer-friendly text on the page** ("fiberless
   component-model-async", "pre-AOT'd to pulley32", byte counts), and it is the first thing the
   terminal shows.
10. **`ok: greeted` vs `success(greeted)`** between the shell and direct runs remains (tracked).
11. **`eo9 store --help` still errors** (round-2 #12, still open).
12. **The `[stderr]` prefix rendering in the page terminal** adds noise: refusals print a
    `[stderr] error: …` line followed by a stray empty `[stderr]` line.
13. **The front page and try-it page spell the same `only` example differently**
    (`eo9:text,eo9:time` vs `eo9:text/text,eo9:time/time`). Both work (good — verified), but the
    participant flagged it as a likely typo and predicted copy-paste failures.
14. **Help text: "a parenthesized argument is passed open, not run"** — meaning never resolved for
    the participant.

### Confusions observed (returning-novice-specific)

- The front page's second sentence ("language-theoretic principles") is still the round-1 filter;
  this participant only got past it because the try-it link was visible without scrolling.
- Operator collision with shell muscle memory (`$` = prompt, `&` = background) caused stumbles on
  every front-page example, though usage in the terminal resolved it.
- "What do I write programs in / is Rust-only forever" — the page's biggest unanswered question;
  determines whether a weekend gets invested.
- The diagnostics import, the save/ls split, and silent `let` all produced "is this broken or am I
  dumb?" moments that one sentence of explanation resolved.

### What landed well (improvements verified relative to round 2)

- **The first-run experience**: seed message with count and store path, straight into a working
  shell; `make help` / bare `make`; the README quick-start order is now correct.
- **The try-it page as the front door**: the explore loop (see → inspect → compose) "is good
  teaching"; bare-default examples; checkable claims; the 1.8 MB honesty.
- **The capability story is now demonstrable end-to-end by a novice**: describe → predict refusal →
  verify; sealing visible via `imports` before/after; attenuation via `greet` passing what `hello`
  fails; reusable environments via `let`; `save` producing a runnable, describable artifact.
- **The error messages on the designed paths**: the `only` refusal, the typed-argument errors, the
  `--fs-root` capability refusal ("the single best thing I've seen in five stages"), and the bare
  `&`-mistake error that names the fix.
- **`env`** — "the best output in the whole system."
- **Determinism claims verified by the participant personally**, twice, including cross-checking
  the page's seed-7 output against a `let`-bound environment.

### Feature requests / asks (the participant's, deduplicated)

- CI-test every shipped example (help text, welcome file, both web pages) — "the one fix".
- A one-line authoring/roadmap statement on the site: programs are Rust today, the format is
  language-neutral, other toolchains can come.
- Annotate no-authority imports in `describe` (e.g. `required eo9:rt/diagnostics (carries no
  authority — exempt from only)`).
- Make the `&`-mistake error teach in all forms (with args, in `-c`).
- A friendlier boot banner (one plain line; details behind a flag/`about`).
- Resolve-failure errors that mention `let` bindings; a confirmation line from successful `let`.
- One outcome spelling across front doors; fs errors brought up to the standard of the capability
  refusal.
- `eo9 store --help` should print help.

### Triage

| # | Finding | Triage | Notes |
|---|---------|--------|-------|
| 1 | Help's own `&` example fails (partial `time.frozen` config refused) | **Fix now** | Either make the example complete (`--monotonic-ns 0`) or make partial configuration fill remaining defaults. Doc fix is one line in eosh's help string. |
| 2 | `/welcome.txt` suggests `wc`, absent from the browser store | **Fix now** | Edit the welcome text or add `wc.cwasm` to the /vm store. |
| 3 | `&` teaching error vanishes when args are present | **Fix now** | Reorder checks so operand-kind is diagnosed before the args-on-`&` rule, or extend that error with the same "use `… $ …`" guidance. |
| 4 | `save` vs `ls /bin` disagreement in the browser | **Needs owner decision** | Either the per-run fs snapshot includes shell-store writes, or `save`'s message shouldn't claim a path that `ls` can't see. Touches the D9/D15 session-overlay design. |
| 5 | Failed-`let` cascade: "cannot resolve `det` (/bin/det.wasm): FsError::NotFound" | **Fix now** | Resolve error should mention bindings ("no such binding or program") and not leak the enum. |
| 6 | `let` succeeds silently | **Fix now** | Print one confirmation line (name + kind). |
| 7 | `eo9:rt/diagnostics` unexplained / silently exempt from `only` | **Fix now** | Annotate in `describe`/`imports` output; one sentence on the try-it page. |
| 8 | fs errors leak `FsError::…` enum text | **Tracked** | GAPS "Error-quality consistency" (R2-18); this session adds the browser resolution-path instances. |
| 9 | Browser boot banner jargon | **Needs owner decision** | The banner is honest provenance info the owner may want; the ask is to humanize the first line and demote the rest. |
| 10 | `ok:` vs `success(…)` outcome rendering split | **Tracked** | GAPS / synthesis #4 (outcome-line unification, owner may veto stderr move). |
| 11 | `eo9 store --help` errors | **Tracked** | GAPS R2-18; trivial fix, repeatedly hit (rounds 2 and 3). |
| 12 | `[stderr]` prefix + stray blank `[stderr]` line in page terminal | **Fix now** | Suppress empty stderr writes; render the prefix more quietly (or color it like the page's vm-error class). |
| 13 | Front page vs try-it page `only` spelling inconsistency | **Fix now** | Pick one form (the short one now works) and use it on both pages — or show both deliberately, with a word saying both work. |
| 14 | "passed open, not run" help wording | **Fix now** | Reword (e.g. "passed as a component value, not executed"). |
| 15 | No authoring/language statement anywhere on the site | **Needs owner decision** | The participant's #2 pain point and the round-2 novice's ask; one paragraph on the front page ("programs are Rust today; the format is language-neutral") — but it's the owner's prose and roadmap commitment. |
| 16 | Front-page jargon ("language-theoretic principles", "fuel-metered", "hardware-root capabilities") | **Needs owner decision** | Round-1's filter is still the second sentence. The try-it page now redeems it for anyone who clicks through; the owner may prefer the prose as is. |
| 17 | Explore-the-sandbox section on the page omits `env` (help's version includes it) | **Tracked** | Already in GAPS as plan/18 D32's deliberate one-line follow-up; this session confirms `env` is the output novices praise most. |

## Facilitator observations

- **Everything shown was real.** The site was served by the real `eo9-www` binary and fetched over
  HTTP; all browser-terminal sessions ran in the committed `/vm` blob through the same JSPI
  read-line path the page uses (`verify-eosh.mjs` passes all 28 checks at this commit); all
  usermode output came from a freshly built `eo9` against an initially empty store. The participant
  drove the command selection from phase 3 onward; the demoer ran whatever they asked, including
  the failures.
- **The two headline bugs were found by the participant, not planned.** The broken help example was
  hit because the participant copied the help's own text (the demoer had only ever typed the
  fully-configured form, as the web page does); the `wc` gap was hit because the participant's
  habit of reading file contents led them to follow the welcome file's instructions. This mirrors
  round 2, where the biggest findings also came from following the product's own instructions
  literally.
- **One presentation caveat**: the page terminal renders one line per `host_write` call; eosh emits
  line text and the trailing newline as separate writes. Whether the lone-newline writes render as
  visible blank lines in a real browser (i.e. whether page output is single- or double-spaced)
  could not be verified in an actual Chrome session (no browser automation was available); the
  transcripts shown to the participant were compacted to single spacing, and this was disclosed to
  them. A click-through on the deployed site is already a tracked GAPS item.
- **The wrong build order still produces a working binary.** The demoer initially built `eo9`
  before `build-guest` (the round-2 trap); the binary works anyway because `seed.rs` falls back to
  the prebuilt `eo9-components` bundle. The round-2 failure mode appears genuinely closed, not just
  documented around.
- **The `&` mistake brief question — "does the new error teach them?" — has a split answer**: yes,
  emphatically, for the bare form (the participant never even hit it, because the error's suggested
  fix is what the page teaches); no for the with-arguments form, which is the form a real user
  types (finding 3).
- The participant agent ran with no tools and no repository access (empty working directory,
  fresh session); its only inputs were the staged texts recorded in this document.
- Study artifacts: the page captures, terminal transcripts, and participant prompts/responses live
  outside the repository (`/tmp/study10-*`); only this report is committed. The scratch transcript
  harness (a copy of `verify-eosh.mjs`'s glue with the participant's input queue) was deleted after
  use.
