# User study 04 — web-platform engineer (eo9.org and the in-browser demos)

## Session metadata

- **Date:** 2026-05-27
- **Branch / worktree:** `study/session-webdev` (worktree of master at `c71ded7`)
- **Participant persona:** a senior web-platform / front-end engineer, 12+ years of experience;
  fluent in performance budgets, HTTP caching/compression, security headers, and the basics of
  WebAssembly in the browser; professionally skeptical of heavyweight wasm blobs and of
  "runs in your browser" claims.
- **Methodology:** the participant was a role-played persona run as a separate session with no
  access to the repository, its documentation, or any tools — it saw only what the facilitator
  pasted into the conversation and replied conversationally. Every command, measurement, and
  program run shown to the participant was actually executed by the facilitator in the study
  environment; outputs are verbatim, trimmed only for length. Failures and stumbles were shown
  as they happened, not cleaned up.
- **Scope:** the eo9.org website only — the static site, the `eo9-www` server's serving behavior,
  the `/try` page (jco-transpiled example components + browser host), and the `/vm` page (the
  wasm32 build of the real runtime stack). The OS itself was described for context but not
  evaluated, except for one native run used to cross-check a claim the `/vm` blob makes.
- **Environment:** the `www/` workspace from this checkout, built with `cargo build --release`
  (~31 s) and `cargo test --release` all green (16 unit + 5 https + 3 limits + 11 server tests);
  the site served locally by that binary in plain-HTTP dev mode on `127.0.0.1:8080`; `curl` for
  headers/sizes; **Node v25.2.1 (V8, JSPI available)** to execute the served demo assets, because
  no driveable retail-browser window was available in the study environment (the browser
  automation extension was not connected). This limitation was disclosed to the participant in
  round 1 and they explicitly asked for a real-browser session as follow-up. The native
  cross-check in round 6 used the release `eo9` binary already built from this checkout earlier
  the same day, with a throwaway `EO9_STORE`. Apple Silicon macOS host.
- **Shape:** six working rounds driven by the participant's asks, a structured wrap-up, and a
  short "what next time" follow-up.

## Round 1 — framing and the participant's opening concerns

The facilitator introduced the project in one paragraph (capability-secure OS on the component
model; imports are the permission set), described the site's three surfaces (front page, `/try`,
`/vm`), stated the demo environment honestly (local serving, curl, Node/V8 with JSPI; no retail
browser drivable this session), and summarized the front page (6.7 KB HTML + 2.7 KB CSS + 695 B
SVG, no JS).

The participant's opening position: credit on sight for a ~10 KB hand-written front page with no
third-party JS or analytics ("better web hygiene than 95% of project sites"); the credibility of
"runs in your browser" lives entirely in the two demo pages; their standing concerns are (1) what
actually ships to the visitor in bytes, (2) how it is served (wasm content-type, compression,
caching, security headers — "for a project whose whole identity is capability security, I expect
the website to have its own act together"), (3) what the demos require of the engine (JSPI).
First asks: verbatim response headers for `/`, `/try`, `/vm`, and the `/vm` blob; a full per-page
asset inventory with on-the-wire sizes, with no hand-applied compression to flatter the numbers.

## Round 2 — headers and asset inventory

Headers shown verbatim from the running server (with `Accept-Encoding: gzip, deflate, br, zstd`
sent on every request): `text/html; charset=utf-8` and `cache-control: public, max-age=300` for
the pages; `application/wasm` and `max-age=86400` for the blob; content-length equal to file
size everywhere. Stated plainly what is absent: **no compression of any kind** (the server
ignores Accept-Encoding; there is no encoder in it), **no ETag / Last-Modified** (so no
revalidation — full re-download when max-age expires), **no fingerprinted URLs**, **no security
headers at all** (no CSP, X-Content-Type-Options, Referrer-Policy, HSTS, COOP/COEP,
Permissions-Policy), HTTP/1.1 only, GET/HEAD only, traversal-safe path resolution with tests,
per-listener connection caps and timeouts.

Asset inventory (exact bytes; on-the-wire = on-disk because nothing is compressed):

- `/try` before running anything: HTML 4,955 + style.css 2,680 + try.css 1,720 + logo.svg 695 +
  try.js 20,026 + host.js 6,184 + manifest.json 3,453 ≈ **39.7 KB**.
- Per program, fetched lazily on first run: hello 174,363 B (141,066 of it jco JS), outcomes
  163,130 B, cruncher 108,512 B, readwrite 278,482 B; whole committed bundle 748 KB; jco output
  unminified.
- `/vm`: HTML 3,119 + style.css 2,680 + vm.css 991 + logo.svg 695 + vm.js 2,938 +
  **web-eo9.wasm 1,069,022** ≈ 1.08 MB up front.
- JSPI: needed only for `readwrite` on `/try` (feature-detected, per-program refusal message);
  the `/vm` blob does not use JSPI.

Participant's reaction, condensed: genuinely good — correct `application/wasm` (streaming
compilation possible), lazy loading, JSPI scoped to the one program that needs it ("exactly the
right call"). Not good, "and ironic for this project": zero security headers ("a first-party-only
static site is the easiest possible CSP to write"), no compression ("likely a 60–70% transfer
reduction left on the table"), and a half-built cache story (after a day, full re-download; a
new blob deploy leaves returning visitors on stale wasm for up to a day — "pick one: immutable
fingerprinted URLs, or validators; right now they have neither"). Headline from the inventory:
hello is 33 KB of wasm and 141 KB of JS glue — "the bytes say the jco JS shim outweighs the
component 4:1." Asks: compressed-size data points; run hello and readwrite via the page's real
assets; show the readwrite manifest entry and break down what is inside hello.js.

## Round 3 — compression numbers, real runs of hello/readwrite, what the jco glue is

Compression (measured with Node zlib; clearly labeled as not what the server does):
`web-eo9.wasm` 1,069,022 → gzip-9 379,081 → brotli-11 **290,543**; `hello.js` 141,066 → 26,507 →
**22,357**; `readwrite.js` 208,294 → 35,978 → **29,736**.

Real runs (Node v25.2.1, the page's own `host.js` imported verbatim, jco modules' `instantiate()`
called the way `try.js` does, core wasm fetched over HTTP from the running local server),
verbatim:

```
try> hello --name browser --excited true
[prog] [1779929337.912000000] Hello, browser!
outcome = success(greeted)
(6.9 ms)

try> readwrite --path /notes/hi.txt --contents "written from wasm"
outcome = success(round-tripped(17))
(7.7 ms)

try> files
  /notes/hi.txt  (17 bytes)  "written from wasm"
```

readwrite went through `WebAssembly.Suspending` (the JSPI path a non-Chromium browser cannot
take). The readwrite manifest entry was pasted verbatim (asyncMain, the three required imports,
typed params, success/failure variants). hello.js breakdown: 4,150 lines, of which 3,731
(~127.8 KB of 141 KB, ~90%) are line-for-line identical to outcomes.js; cruncher.js (a component
with no imports) is still 93 KB — the approximate floor of the generic jco preamble; per program
the visitor ships ~30–70 KB of core wasm plus ~90–200 KB of glue that is mostly the same runtime
repeated, unminified; no base64-embedded wasm.

Participant's reaction: the demo runs are credible; the manifest generated from the component's
WIT and the refusal-before-instantiation launcher are "the right shape — the page is showing the
import set as data, not as marketing copy." The bundle problem is now quantified: a visitor who
tries all four programs downloads ~570 KB of JS of which ~450 KB is the same bytes four times;
the fix stack (shared intrinsics, minify, brotli) "is entirely build/serving work, not
architecture." Their core credibility question: is grant/revoke a genuine absence of the
capability at `instantiate()` time, or "a UI-level flag … theater that a skeptic will see through
in five minutes of devtools"? Asks: show the code where grants are applied; revoke fs, run
readwrite, then bypass the launcher and show the real failure; can a component get something the
host didn't grant?

## Round 4 — grant/revoke: the mechanism, the bypass, the framing

Code shown: `host.js`'s `makeImports()` (capability interfaces only added to the import object if
granted; types-only interfaces and the buffer resource always present) and `try.js`'s
`runProgram()` (the friendly "refused before execution" pre-check over manifest metadata, then
`makeImports(state.grants, …)` → `instantiate()`).

Run for real with fs revoked (the launcher's refusal logic was copied verbatim into the driver
because `try.js` cannot be imported outside a browser — it touches the DOM at module top level;
`host.js` was imported unmodified):

```
(a) the launcher path — what /try prints for `readwrite ...` with fs revoked:
refused before execution (the loader rule — nothing was instantiated):
  missing required import eo9:fs/fs@0.1.0 — the "fs" capability is not granted (try `grant fs`)

(b) bypassing the launcher check: calling the jco module's instantiate() with the
    same revoked grants (host.js builds the import object, fs absent):
    import object keys handed to instantiate(): ["eo9:io/buffers","eo9:text/types","eo9:time/types","eo9:fs/types","eo9:text/text","eo9:time/time"]
    threw TypeError: Cannot destructure property 'File' of 'imports.eo9:fs/fs' as it is undefined.
```

(The equivalent for hello with time revoked had been run earlier: `TypeError: Cannot destructure
property 'default' of 'imports.eo9:time/time' as it is undefined.`) Stated precisely: the
enforcement point is the JS host constructing the import object; the friendly refusal is ~30
lines of launcher JS over build-time metadata, and the page's "about" text says it is a launcher
but does not spell out that distinction. The host only ever provides what is granted; the
manifest is metadata for the pre-check and UI, not the enforcement.

Participant's verdict on `/try`: "the grant/revoke mechanism passes my bar … the (b) bypass test
showing a hard TypeError rather than a soft 'permission denied' is the proof." The framing needs
one more honest sentence on the page: "the polite message is the launcher; the actual enforcement
is that the import simply isn't there — write it down." Architecture credible; the bytes need the
build/serving work. On to `/vm`, "where my skepticism is naturally highest … exactly the shape of
demo that can be either genuinely impressive or pure replay theater."

## Round 5 — /vm: what it claims, what it does, real runs with timings and memory

Explained from the page copy and the blob/build sources: at build time the three demo components
(the kernel's seed component, a fuel-metering variant, and the unmodified `entropy.seeded` stub)
are pre-compiled to Pulley bytecode images and embedded in the blob; the blob is the project's
pinned wasmtime 45.0.0 (vendored) built for wasm32 with the Pulley interpreter and the fiberless
async path; the 1,069,022-byte file is committed. At click time the blob builds an Engine,
`Component::deserialize`s the embedded image (no compilation in the browser), creates a Linker,
instantiates, and makes real typed calls (hello/add; the same with a fuel budget and a fuel-spent
report; async-lifted configure + N get-u64 draws through the same concurrent call path the native
runtime uses). Honest caveats given unprompted: the Linker is empty — all three components import
nothing, so no capability wiring is exercised; no composition, store, or compiler in the browser;
components baked into the blob. What `/vm` proves today is "the real engine + the real component
ABI run inside a browser wasm engine," not "Eo9's capability model runs in your browser."

Run for real (Node/V8, `instantiateStreaming` with the single `env.host_write` import, fetched
from the local server), abridged here — the full output is in the round transcript above the
findings: fetch 31.9 ms on loopback, compile 1.0 ms, instantiateStreaming 8.0 ms; rss 90.4 MB
after instantiation (105.6 MB after all runs); blob linear memory 1.3 → 5.1 MB; `boot()` reports
the wasmtime version and embedded artifact sizes (seed 3,272 B, seed+fuel 3,656 B,
entropy.seeded 119,560 B); `run_hello()` → `hello() -> "Hello from a WebAssembly component on
bare-metal Eo9!"`, `add(17, 25) -> 42` (9.6 ms); `run_fuel()` → 8 fuel units for the call;
`run_entropy(0xe09, 4)` → four 64-bit draws, identical on a second click, different with a
different seed (0x1234abcd → two different draws). Two facilitator flags: the only number a
visitor really feels is 1.07 MB uncompressed before anything is clickable; and the blob's closing
line "same seed, same sequence — on this page, on native Eo9, and on the bare-metal kernel" is
printed by the thing being demonstrated and had not been independently checked yet.

Participant's reaction: "a much better /vm story than the shape of the demo suggested … the blob
is doing real work … the seed-changes-output evidence says it's computing, not replaying"; the
deserialize-pre-AOT decision "is the right engineering call." But: it proves the engine and ABI
run in a browser and "demonstrates **nothing** about capabilities — the linker is empty … For a
project whose one-line identity is 'the import set is the permission set,' the page that carries
the OS's name currently exercises zero of that"; the empty-linker fact "needs to be stated
explicitly … much better as a disclosure than a discovery." `/try` and `/vm` each have half of
the story; the site should say so crisply. They declined to accept the determinism line on the
blob's say-so and took up the offer of a native cross-check; also asked what the visitor sees
when `instantiateStreaming` fails, and for the "not here yet" passage verbatim.

## Round 6 — the native cross-check, the failure path, the disclosure text

Native cross-check, including the stumble: the first attempt against the facilitator's default
store failed (`error: cannot resolve `rng` (/bin/rng.wasm): FsError::NotFound`, exit 1 — a quirk
of that store's contents, shown as it happened). With a throwaway `EO9_STORE` and the unmodified
`entropy.seeded` stub + `rng` program added, the native runs produced:

```
eo9 -c "entropy.seeded --seed 3593 $ rng --count 4"        # 0xe09
5791370169577441206
16348740061181361748
16371684761001030115
8074310053562519654
ok: generated(4)            [exit 0]

eo9 -c "entropy.seeded --seed 305441741 $ rng --count 2"   # 0x1234abcd
14753254011098083745
15794789996577110731
ok: generated(2)            [exit 0]
```

These are exactly the blob's hex draws from round 5 converted to decimal — value-for-value
matches on both seeds. The bare-metal leg of the claim was explicitly left as "not checked here."
The vm.js failure path: load wrapped in try/catch; on failure the output box shows "could not
load the Eo9 blob: <error>" plus a hard-coded "(this page needs a browser with WebAssembly
enabled)" line, and the buttons (disabled by default in the HTML) stay disabled — an explained
error, not silently dead buttons, but the canned diagnosis line can mislead when the real cause
is a 404 or a header problem. The `/vm` "not here yet" passage was pasted verbatim; it lists the
shell, the algebra, the store, and blocking I/O, and does not mention the empty-linker fact or
performance expectations.

Participant: "That settles it for me — the cross-check is a genuinely strong result … exactly the
kind of evidence that makes 'the same runtime stack runs in your browser' a claim I'd repeat
rather than roll my eyes at." The store stumble "oddly adds credibility." Their prioritized list
going into wrap-up: (1) serve compressed, (2) security headers, (3) fix the cache story,
(4) de-duplicate the jco glue, (5) the two disclosure sentences (one for `/try`, one for `/vm`).

## Wrap-up (the participant's structured answers, condensed, their words where quoted)

**Top 3 pain points**
1. No compression anywhere — 1.07 MB of wasm and ~570 KB of jco JS raw on the wire when brotli
   cuts them 73–84%. "It's the only number a visitor actually feels."
2. Zero security headers on a site whose entire pitch is capability security — "trivial to fix
   and embarrassing to be caught without."
3. The half-built cache story: day-long max-age, no validators, no fingerprinted URLs — full
   re-downloads and a stale-blob window after every deploy.

**Top 3 missing things**
1. A shared/minified jco intrinsics module on `/try` (~90% of each program's JS is the same
   preamble shipped four times).
2. The two disclosure sentences: `/try`'s refusal message is launcher JS (enforcement is the
   absent import); `/vm`'s components import nothing, so no capability mediation is exercised
   there.
3. Any stated browser-support / performance expectations: a support matrix, and
   interpreter-speed expectations for `/vm`'s future.

**Credibility bar**
(a) For "runs in your browser": serve it compressed, publish asset sizes honestly, keep the JSPI
caveat, state on `/vm` what is and isn't exercised — "the execution evidence itself already
cleared my bar." (b) For the web presence overall: security headers shipped, the cache story
fixed, and the bare-metal leg of the determinism claim either verified publicly or softened to
what has actually been checked.

**Genuinely impressed**
The grant/revoke enforcement being a real absence in the import object ("the bypass test threw,
it didn't politely warn"); the manifest generated from the component's WIT driving the launcher;
the `/vm` blob doing real deserialize-and-call work with the actual async call path; its entropy
sequences matching the native runtime exactly on two seeds; the site's restraint (no framework,
no third-party JS, ~10 KB front page, lazy-loaded demos, disabled-until-boot buttons with
explained failures).

**Mis-framed (not just unfinished)**
`/vm` carrying the OS's name while exercising none of the capability model, without saying so;
the vm.js error text hard-coding "needs WebAssembly enabled" regardless of cause; the blob
printing its own cross-host determinism claim as fact "when only the page itself is in evidence."

**One engineering week, if that's all the website gets before an announcement**
Days 1–2: build-time brotli/gzip pre-compression + server support, security headers,
fingerprinted URLs or ETags. Day 3: shared jco intrinsics + minification on `/try`. Day 4: copy
pass — the two disclosure sentences, support expectations, fix the error text. Day 5: verify the
bare-metal determinism leg and publish the comparison. Explicitly not: HTTP/2/3, a service
worker, design polish, or new `/vm` features — "the existing demos plus honest serving is a
stronger announcement than one more button."

**Wanted next session (their list, in their priority order)**
A real browser window (cold-cache throttled waterfalls, paint/interaction timings, confirming
streaming compilation actually streams); a Safari/Firefox + mid-range Android pass (the JSPI
refusal as a visitor sees it; blob memory/instantiate time on weak hardware); the production
deploy/TLS story (real cert chain, what rustls negotiates, mild concurrent load while serving a
1 MB asset); the front-page copy versus what the demos can back up; accessibility and no-JS
behavior; how the committed demo artifacts are regenerated and whether they can drift from the
repo they claim to represent.

## Findings

### Verified during the session (server and pages behaving as built)

1. **No compression.** The server ignores `Accept-Encoding` entirely; every asset ships at its
   raw size. Measured headroom: `web-eo9.wasm` 1,069,022 B → 290,543 B brotli-11 (379,081 gzip-9);
   `hello.js` 141,066 → 22,357; `readwrite.js` 208,294 → 29,736.
2. **No security headers.** No CSP, X-Content-Type-Options, Referrer-Policy, HSTS, COOP/COEP, or
   Permissions-Policy on any response (verified on `/`, `/try`, `/vm`, the blob).
3. **No revalidation or fingerprinting.** Only `Cache-Control: public, max-age=300` (HTML) /
   `max-age=86400` (assets); no ETag or Last-Modified; stable asset URLs. Consequences: full
   re-downloads at expiry and an up-to-a-day stale-asset window after any deploy that changes the
   blob or bundle.
4. **jco glue duplication and weight on `/try`.** ~90% of hello.js (3,731 of 4,150 lines,
   ~127.8 KB of 141 KB) is byte-identical to lines of outcomes.js; cruncher.js (no imports) is
   still 93 KB; output is unminified; per-program transfer is ~108–278 KB against 15–70 KB of
   actual core wasm. All four programs together: ~570 KB of JS, most of it the same preamble
   four times.
5. **The `/try` refusal message is launcher JS.** The friendly "refused before execution (the
   loader rule…)" text is produced by ~30 lines of page JS over build-time manifest metadata.
   The underlying enforcement is real — a revoked capability is genuinely absent from the import
   object, and bypassing the check yields
   `TypeError: Cannot destructure property 'File' of 'imports.eo9:fs/fs' as it is undefined` —
   but the page does not state the distinction.
6. **The `/vm` page never states that its components import nothing.** The Linker is empty for
   all three demos, so the page exercises the engine and the (sync + async) component ABI but
   none of the capability model; the "not here yet" list does not include this, and the page sets
   no interpreter-speed expectations.
7. **The blob asserts its own determinism claim.** "same seed, same sequence — on this page, on
   native Eo9, and on the bare-metal kernel" is printed by the blob. The native leg was verified
   in this session (value-for-value match on seeds 0xe09 and 0x1234abcd); the bare-metal leg was
   not checked.
8. **vm.js error handling.** On any load failure the page appends a hard-coded "(this page needs
   a browser with WebAssembly enabled)" line even when the real cause is a 404/network/header
   problem (the actual error is printed on the line above). `instantiateStreaming` has no
   ArrayBuffer fallback (currently fine because the content-type is correct).
9. **Protocol/limits posture** (as designed, but relevant to a public site serving a 1 MB asset):
   HTTP/1.1 only, GET/HEAD only, 256-connection cap per listener, 10 s header timeout, 60 s
   connection lifetime. The participant explicitly deprioritized HTTP/2/3.
10. **Facilitator's first native cross-check attempt failed** because the machine's default store
    had no `rng` binding (`cannot resolve `rng` … FsError::NotFound`); a throwaway store with the
    stub + program added worked. Not a website issue; recorded because it was shown.

### What landed well

- Correct `Content-Type` everywhere, including `application/wasm` (streaming compilation works)
  and UTF-8 charsets; clean 404s; careful traversal handling backed by the test suite; the whole
  `www` test suite (35 tests) green on the release build used for the session.
- The `/try` page's architecture: ~40 KB before any program runs, per-program lazy loading,
  manifest generated from the components' actual typed interfaces, typed flags and typed
  outcomes rendered as data, JSPI feature-detection scoped to the single program that needs it.
- Grant/revoke is a real mechanism in the browser host: the capability interface is simply not in
  the import object when revoked; the components ran with real behavior (timestamped hello,
  fs round-trip visible in the page's `files` listing) at millisecond instantiate+run times.
- The `/vm` blob does real work in the browser engine: deserialize of pre-compiled Pulley images,
  typed calls, fuel accounting, and the async-lifted call path — with output that depends on
  inputs, and an entropy sequence that matches the native runtime exactly.
- Honest labeling already present on both demo pages ("this prompt is a launcher, not eosh";
  "Work in progress"; the "not here yet" list), disabled-until-ready buttons, explained load
  failures, and no third-party JavaScript anywhere on the site.
- The site's overall restraint and weight (front page ≈ 10 KB total) — called out by the
  participant unprompted in round 1 and again at wrap-up.

### Asks / feature requests from the participant

- Pre-compress assets at build time (brotli + gzip) and serve them (`Content-Encoding`
  negotiation); publish the real transfer sizes on the demo pages.
- Add the basic security headers: CSP (first-party-only is easy here), X-Content-Type-Options,
  Referrer-Policy, HSTS in the TLS modes; consider COOP/COEP only when/if threads need it.
- Fix caching: fingerprinted immutable asset URLs or ETag/Last-Modified validators (either is
  acceptable; currently there is neither).
- Split the shared jco intrinsics out of the four per-program modules and minify the output
  (target: per-program cost ~40–80 KB instead of 175–280 KB).
- Add the two disclosure sentences: on `/try`, that the polite refusal is the launcher and the
  enforcement is the absent import; on `/vm`, that the current demo components import nothing so
  no capability mediation is exercised yet.
- State browser support (a small matrix) and interpreter-speed expectations for `/vm`.
- Verify the bare-metal leg of the determinism claim and publish the comparison, or soften the
  blob's printed claim to what has been checked.
- Make the vm.js failure message reflect the actual error instead of always blaming missing
  WebAssembly support.
- Explicitly deprioritized by the participant: HTTP/2/3, a service worker, visual redesign, new
  `/vm` features.

## Facilitator observations

- No retail browser was drivable in this environment, so "in the browser" evidence was produced
  by executing the served assets in Node/V8 (which has JSPI) and by reading the page sources;
  the participant accepted that for bytes/behavior but listed a real-browser pass (throttled
  waterfalls, Safari/Firefox/Android, paint timings) as their top item for a follow-up session.
  The project's own plan notes record the same gap for `/vm` (the in-retail-browser run was not
  captured when the page landed), so that verification item is still open on both fronts.
- `host.js` imports cleanly outside a browser; `try.js` does not (it touches the DOM at module
  top level), so demonstrating the launcher's refusal text required copying two of its functions
  verbatim into the test driver. A small separation (pure launcher logic vs. DOM wiring) would
  make the refusal path unit-testable and reusable headlessly.
- The repository's status documents still describe the wasm32 real-stack browser blob as
  "deferred" (STATUS.md "Settled (see GAPS)" note and the GAPS entry, dated 2026-05-26), but the
  `/vm` page and `www/web-eo9` landed afterwards and are in the tree this study ran from. Stale
  status text is exactly the kind of thing earlier study participants flagged as a credibility
  risk; worth a one-line refresh.
- Server behavior matched the `www/README.md` claims in every respect checked (content types,
  cache headers, methods, traversal handling, limits); the gaps the participant found
  (compression, validators, security headers) are absent from both the code and the README — the
  documentation does not overclaim here, it just shares the same blind spots.
- The numbers in this report are from a release build on an Apple Silicon laptop over loopback;
  they are useful for "what work happens" (compile/instantiate/run times, memory) but say nothing
  about real-world load experience, which is dominated by the uncompressed transfer sizes.
