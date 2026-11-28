---
type: decision
links:
  - rel: part-of
    to: CLAUDE.md
    note: the Rex direction and its upstream watch list, which CLAUDE.md's Intended architecture points into
research:
  bears-on: >-
    The zero-emulator direction: warden as the client for Rex sessions, libghostty as its
    terminal, tmux and amux retired. News is anything that moves the unlock in "Not yet: warden
    as a Rex client", changes Rex's API/events/pricing, or changes the libghostty embedding API
    (`ghostty.h`) or the terminal-state pieces (snapshot, OSC 7501) warden consumes.
  sources:
    - name: Superlogical devlog
      check: https://www.superlogical.com/updates/rss.xml
      note: the bare domain 308-redirects to www (curl -L); the Rex docs serve raw Markdown at /rex/docs/<page>.md
    - name: Hashimoto's and Superlogical's X posts
      check: curl -A <browser UA> https://api.fxtwitter.com/2/profile/<mitchellh|superlogical>/statuses (one post, quote included, at /status/<id>)
      note: >-
        The maintainer blocks X at DNS, deliberately, so x.com, pbs.twimg.com and d.fxtwitter.com
        answer with a certificate mismatch; don't route around it. X is the primary channel. The
        hachyderm.io API (browser user-agent, /api/v1/accounts/lookup?acct=mitchellh) is only a
        partial mirror and missed every post from 10-11 to 10-15. Post media (charts, images) sit
        on pbs.twimg.com with no alt text, so an image-only post stays unread unless a non-X
        mirror (hachyderm, the devlog, Discord) carries it.
    - name: Other Superlogical staff on X
      check: the fxtwitter route above, for almonk (Alasdair Monk) and pearkes (Jack Pearkes)
      note: Activity Mode and beta-wave news surfaced through these accounts before the company's
    - name: Hashimoto's blog
      check: https://mitchellh.com/feed.xml
      note: the OSC 7501 rationale and the Ghostty-leaving-GitHub plan were posted here, not on the devlog
    - name: Rex docs
      check: >-
        the page list in https://www.superlogical.com/rex/docs/llms.txt (whole text at
        llms-full.txt); https://www.superlogical.com/rex/docs/search.json?q=attach (also
        q=protocol) shows whether the unlock has been documented
      note: docs change without a devlog post
    - name: Beta Discord (community MCP)
      check: >-
        user-scope MCP `superlogical`: list_changelog and read each post since the as-of date;
        list_activity since the as-of date; search_feedback for data.connect, protocol, snapshot,
        program_status / OSC 7501, split ratio, restore and endpoint schemes (http, ssh)
      note: >-
        the MCP (superlogical.com/rex/community-mcp) carries the beta Discord's feedback, #backlog
        and #changelog. It signs in through a Discord account that is in the test program, so an
        agent can read it only after a human has authenticated it (`/mcp`)
    - name: Superlogical GitHub org
      check: gh api orgs/superlogical/repos --jq '.[].full_name'
      note: the org exists with no public repos; a first repo is the likely home for code, a protocol spec or a license
    - name: YouTube devlogs
      check: yt-dlp --flat-playlist --playlist-end 5 --print '%(id)s %(title)s' https://www.youtube.com/channel/UC0gjVbm7HY5GzDTo5NbQruA/videos
      note: >-
        the channel's RSS feed (/feeds/videos.xml?channel_id=…) returns 404 with or without a
        browser user-agent, and the page is an SPA; the flat listing gives no upload dates, so
        compare ids against the ledger. Transcripts via yt-dlp --write-auto-subs (the
        cookie-rotation warning is noise). The @Mitchellh3 handle does not resolve.
    - name: Hacker News
      check: https://hn.algolia.com/api/v1/search_by_date?query=superlogical&tags=story (also query=rex, query=OSC 7501)
      note: news.ycombinator.com rate-limits; items via /api/v1/items/<id>
    - name: Ghostty releases
      check: gh api repos/ghostty-org/ghostty/tags --jq '.[:3][].name'
      note: >-
        the GitHub "release" is only the rolling tip; tags carry the versions. warden vendors a
        main commit, not a release, so the main compare below is the source that matters
    - name: libghostty embedding API since warden's vendored commit
      check: >-
        gh api repos/ghostty-org/ghostty/compare/<commit>...main for include/ghostty.h,
        src/apprt/embedded.zig, src/terminal/snapshot/; <commit> is in warden's
        crates/warden-app/vendor/PROVENANCE.md
      note: >-
        warden follows main, so a fix here that warden uses is a revendor in the same session
        (warden's docs/surface.md), never a wait for a tag
    - name: libghostty PRs (snapshot, OSC 7501, multiplexer)
      check: gh search prs --repo ghostty-org/ghostty --merged-at '>=<as-of>'
      note: >-
        Ghostty plans to leave GitHub (mitchellh.com, 2026-04-28; destination not yet named),
        keeping a read-only mirror at the current URL. Once it moves, this search goes silent,
        while the tag and compare checks keep working on the mirror. A sudden empty result means
        the project has moved, not that work has stopped
    - name: OSC 7501 in the layers between agent and warden
      check: >-
        gh search issues --repo tmux/tmux 7501 and gh api repos/tmux/tmux/tags; grep 7501 in
        https://raw.githubusercontent.com/anthropics/claude-code/main/CHANGELOG.md and in gh
        release list -R openai/codex
      note: >-
        whether an agent under amux emits at all depends on tmux answering the query and on
        the agent's emit rules (see "Agent status reaches warden from Rex")
    - name: Press
      check: web search "Superlogical Rex" (The Register, InfoWorld, XenoSpectrum have covered it)
---

# Multiplexer direction — warden and Rex replace tmux and amux

Durable record of the architectural direction, and the external evidence behind it.
Start here before building on a Rex API, revendoring libghostty for Rex, or proposing new
architecture for amux, the dormant tmux-based agent launcher this direction retires.

**Transcribed, not linked, on purpose.** The primary sources are YouTube videos and X threads,
which agent tooling reads only through the routes in the frontmatter's `research:` watch list,
so a bare link would be a dead end for the next agent. The ledger's last entry carries the as-of
date for everything below it; a status check runs the watch list from there.

## What Superlogical is doing

Superlogical (Mitchell Hashimoto, announced 2026-07-29) is building a server-side
terminal multiplexer on libghostty. The parts that bear on amux, from the
*Superlogical Terminal Multiplexer High-Level Architecture* devlog:

- **No emulator in the middle.** A traditional multiplexer sits between the terminal
  emulator and the PTY, so terminal bytes are parsed twice and screen state is
  materialized twice. Superlogical's server owns the authoritative session state and
  **tees raw PTY bytes to every client**, rather than sending screen diffs the way
  tmux/zellij/screen do. Clients parse independently — "synchronized distributed finite
  state machines" — so a slow server does not slow a client's rendering, and a
  miscompiling client corrupts only its own view, never the authority.
- **Attach is a snapshot frame, then the raw stream.** On connect the server pauses PTY
  processing, sends a custom binary protocol frame carrying just enough state to render
  (visible screen, dimensions, cursor/mouse), then a `ready` frame; the client is
  interactive at that point. Scrollback streams in afterwards, newest-to-oldest, showing
  a loading state until it lands.
- **Input is serialized to the authority** — one writer, many readers.
- **The client owns the viewport.** Per-client scroll and selection. tmux's shared
  scroll across attached clients is called out by name as the thing this fixes.
- **Splits are native and one-to-one with a PTY.** Each split is a native tab / window /
  split with **its own connection**; there is deliberately no multiplexing *within* a
  window. Native apps ship for macOS/iOS, web and mobile.
- **A compatibility mode is kept, not avoided.** For a terminal that does not speak the
  protocol, Superlogical puts a libghostty terminal in the middle and accepts that this
  is "architecturally identical" to a traditional multiplexer.
- **The binary protocol is open.** "If it's not libghostty, it's still a binary protocol
  anyone can parse," and it lives predominantly in MIT-licensed libghostty. What the
  company sells is the control plane, not the wire format — and, per the 2026-09-02 server
  devlog in the ledger, not the server either: that is self-hosted.
- **Feature velocity is an explicit part of the argument**, and tmux's lack of kitty
  graphics is the case study Hashimoto uses for it.

Treat all of the above as design intent, not a shipped contract — nothing exists to adopt
yet. The ledger below is the record of what has and hasn't happened; append to it rather
than re-deriving the state from the web.

### Status ledger

- **2026-07-29** — company announced (`mitchellh.com/writing/superlogical`); the beta
  waitlist opens at `superlogical.com`. Co-founders: Jack Pearkes (HashiCorp's first
  employee), Alasdair Monk, Hector Simpson. US$10M seed led by Notable Capital with Amplify
  Partners; angels include Patrick Collison, Tobi Lütke, Aaron Levie, Guillermo Rauch and
  Armon Dadgar. It is a venture-scale play, so the paid layer is the lock-in risk; which layer that is was
  narrowed on 2026-09-02, below.
- **2026-07-30 → 08-01** — the architecture devlog and X thread transcribed above.
- **2026-08-28** (X; the Hacker News mirror *Basic Superlogical Demo* is dated 08-30, and a
  YouTube upload of the same three minutes, titled *Superlogical Pre-Alpha Demo*, followed
  on 09-02) — first public demo: the macOS app, basic functionality only, pitched on speed.
  What it shows: a sub-half-second cold launch into an interactive session, and the same
  after quitting the client and reopening it against several live workloads (btop, htop,
  a counter that kept counting while the client was closed); named sessions ("work"),
  created and switched from the client; **native scrolling with a native scroll bar**, the
  client-owns-the-viewport point made visible; vertical tabs exist alongside horizontal.
  The server in the demo is local. Hashimoto's clarifications alongside it: not macOS-only;
  the web client is "very functional" and gets its own demo later; which platforms are
  stable enough for the *initial* public release is still undecided; and (08-30) "all the
  terminal-specific work is going into libghostty so everyone can benefit, e.g. the
  terminal binary snapshot."
- **2026-09-02** — *Superlogical Server Memory Optimizations Overview* (X post with four
  charts + an 11-minute devlog, YouTube `T5gV6anSt-4`). Two things it settles:
  - **The server is self-hosted, not a service.** Hashimoto's words: "when I say server,
    it's a fully self-hosted thing. It's not a service. It's built into the Mac app" — a
    fresh install starts a local server invisibly, and the same server binary "runs
    anywhere", Linux included. The stated vision is a server on every machine, host and
    pod you have. So the paid layer is the control plane (accounts, sharing, remote
    resolution), not the thing the PTYs run in.
  - **Benchmarks exist, and they are about memory at agent scale.** The scale argument:
    agents spawn terminals and attach as clients in numbers no multiplexer was designed
    for, so the target is "hundreds of people, hundreds of thousands of agents". Numbers
    (macOS 26.6.2, Apple M4 Max, `phys_footprint`; zellij "default" is out-of-the-box,
    "plugin-free" removes all plugins; lower is better):

    | Measure | tmux 3.5a | Superlogical | zellij 0.45.0 plugin-free | zellij 0.45.0 default |
    | --- | --- | --- | --- | --- |
    | Server start, one session | 2.50 MiB | 10.6 MiB | 35.3 MiB | 53.6 MiB |
    | Per empty 80×24 terminal | 15 KiB | 68 KiB | 2.30 MiB | 7.88 MiB |
    | Per 80×24 terminal filled with 10,000 lines | 4.89 MiB | 407 KiB | 22.7 MiB | — |
    | Per client connection, 50 filled terminals | 157 KiB | 85 KiB | 21.7 MiB | 1.56 GiB |

    Hashimoto's own reading: tmux's empty states are "excellent" and he expects to match
    them; Superlogical wins once terminals have content or clients attach; zellij's
    per-client cost comes from its default tab/status bar being wasm plugins instantiated
    per tab, one runtime each, and not reclaimed on close. The mechanisms behind the
    Superlogical column, because two of them are libghostty capabilities rather than
    server ones:
    - **Terminal parking.** A terminal idle for 60 s on *PTY read* (typing into it does
      not count) has its entire emulator state — screen, scrollback, modes — written to
      disk as an encrypted **binary snapshot**, and is then kept alive only by a
      file-descriptor watch. This happens with clients attached: ten idle shells in an
      open app are ten parked terminals. Unparking a 64 MB compressed scrollback is quoted
      at ~200 µs excluding disk, decoded streaming. A client attaching to a parked
      terminal is served the snapshot straight from disk without unparking. The
      snapshot/restore is libghostty's ("the only terminal technology within a multiplexer
      that supports that"), which is the capability the 08-30 reply above says lands
      upstream.
    - **PTY parking.** Throughput comes from one blocking OS thread per PTY (re-verified:
      pooling PTY fds into kqueue/epoll/io_uring costs measurable latency — Ghostty is the
      reference). At server scale that thread is the expensive part, so an idle *or
      unobserved* PTY is moved into a single evented poller (a 5–10 % throughput cost
      taken only when no human is watching) and moved back on activity.
    - **Client buffer parking.** The per-client pipeline buffers are freed once a client
      goes idle after its initial sync and reallocated on the next burst.

    The follow-ups he named for the same harness: IO throughput, CPU, security (the
    snapshot encryption, "secrets in scrollback"), and GNU screen added as a comparison
    (09-03, on request).
- **2026-09-08** — *Superlogical Remote Sessions Pre-Alpha Demo* (X post + a 6-minute
  YouTube video, `PdwTjSBW6Y8`; transcript pulled the same way as the others). The first
  look at the remote path, and it answers the `amux @host` question more sharply than the
  09-02 "runs anywhere" line did:
  - **The server replaces sshd, it does not tunnel through it.** The multiplexer server is
    named **Rex** in the demo. Adding a remote host (from the client's command palette) makes
    the server perform "a full SSH-style system login" — it shows up in `who`, the login
    shell and per-user limits apply — and Hashimoto runs the demo VPS "with no SSH on this
    machine at all". Identity comes from the transport: the demo connects over Tailscale and
    `whoami` reports the Tailscale identity, the hops taken, and the user being acted as; a
    server-side mapping decides who may act as whom (root included), and pre-existing SSH
    keys are honoured as a method.
  - **One connection, many terminals.** Splits on the remote host ride the same connection.
    Every new session gets a generated two-word name (`drifting-cedar`), renamable; sessions
    and their kill propagate to every attached client at once, and quitting and reopening the
    client lands back in the last session, local or remote.
  - **Mosh-shaped transport, details withheld.** "A lot of the architectural similarities as
    Mosh", "not everything Mosh does", explicitly not ready to describe the protocol or
    transport. The pitch is a cross-country server that feels local.
  - **A CLI is injected into every session**, local and remote (`whoami`, session create /
    kill by id); and a **go-to-directory** palette action (⌘⇧G) opens a new terminal in a
    chosen directory, with the directory listing served over the remote connection too — the
    mechanism is deferred to a future architecture devlog. The stated design rule is that
    everything local works remote and vice versa.
- **2026-09-14** — *Superlogical Pre-Alpha Demo: CLI* (YouTube `9fcfDF8SBnc`, 7½ minutes;
  cross-posted to X/Mastodon the same day, no HN mirror). The CLI binary is **`rex`**,
  the same name as the server. Its data model, as shown: a **session** holds **windows**
  (tabs), a window holds **blocks**, and each block has a type (`terminal` is the only one
  shown), an id and a label (`shell` is the default label). What the demo does with it:
  - **Create and arrange from outside the client.** `rex` creates a named session running a
    command, adds a labelled block to it (it lands as a split), and **moves** a block relative
    to another ("move nvim below shell") without disturbing the running process; `rex block
    close` and session kill work too. Every change shows up at once on every connected client.
  - **Scripting primitives.** `wait` blocks until a named block's process completes; a
    `--keep-open` flag keeps the finished block on screen (by default it disappears).
  - **Introspection.** `inspect` on a session reports its id, label, focused window, window
    count and **connected clients** (the CLI and the Mac app each count as one); block listing
    and block inspect return ids, types, labels and metadata.
  - **Every block exposes its own methods and events.** A terminal block's `process` method
    returns the child process and, separately, the **foreground process** (a shell running
    nvim reports both), or exit information once it has exited. A per-session **event stream**
    emits client-connected, block size-changed, block-closed, layout-changed and, from the
    terminal block itself, **child-exited**.
  - **More than the GUI exposes.** Per the help output: send input, simulate key presses and
    mouse movement, and capture the screen as text, HTML or other formats. Hashimoto's claim is
    that anything the graphical client does, the CLI does, plus more; he pitches it at editors
    and agent tools.
- **2026-10-05** — *Public testing is beginning* (`superlogical.com/updates/public-testing-beginning`).
  The product is named **Rex** — the server and CLI names from 09-08/09-14 are the product's.
  Invites go to the mailing list in growing waves, the whole list "in less than a month";
  testing is **macOS app only** (it self-hosts its own Rex server and connects to other Rex
  servers). Rex servers already run on Linux and Windows and clients exist for iOS, Linux and
  Windows "at varying stages of readiness", outside the test. ~50 private testers before this.
  Pricing: "Rex is and will always be free" — no account, fully self-hostable, no data seen; the
  Rex clients and servers "are not going to be directly monetized". The commercialization plan
  is unannounced, promised before the stable release, which will be a contact-free download.
  Still no public code, wire-protocol spec or license.
- **2026-10-06** — **OSC 7501, the Program Status Protocol** (spec rev 0.2 at
  `superlogical.com/rex/docs/build/program-status`, rationale at
  `mitchellh.com/writing/program-status-osc7501`). The first *published* protocol, but a terminal
  escape sequence, not the Rex wire protocol: a program reports its own state to the terminal
  over the pty — `OSC 7501 ; state=…:kind=…:app=…:progress=…:msg=<b64> ST`. States `idle` /
  `working` / `done` / `blocked` (`kind=permission|question|auth`) / `error`, plus `clear`; each
  report replaces its record whole; hierarchical `id`s (`build/test`) give a program several
  records at once. `working`/`blocked` drop on process exit or the next OSC 133 A prompt;
  `done`/`error` survive both until the terminal decides the user has seen them. Detection is the
  `OSC 7501 ; ? ST` echo (send `CSI c` after it to avoid a timeout).
  Implemented in Rex and in **libghostty-vt** (ghostty-org/ghostty#14560); the embedding API in
  `ghostty.h` has no action for it, so the Ghostty app ignores it and warden can't see it.
  Proof-of-concept emitters exist (plugins/forks) for Claude Code, Codex, Terraform and Homebrew,
  nothing upstreamed. Hashimoto's pitch is the
  "agentic inbox" — he counts 250+ orchestrators guessing agent state from titles/screens
  (Herdr's 16 Claude Code title-regex rules as the example) — which is exactly what amux's
  probe and warden's attention/presence dots do today. Rex shows it as session-picker
  indicators, spinners/blocked glyphs on unfocused tab headers, and a Lua
  `terminal.program_status_changed` event; a push-to-mobile inbox is planned. Unlike OSC 9/777 it
  is state, not a one-shot event, and the pty carries it over ssh and through containers with no
  socket bridging — though through tmux it still needs passthrough per layer, like OSC 777.
- **2026-10-07** — two X/Mastodon posts, no devlog.
  - **OSC 7501 adoption, 24 h in:** integrated in Amp, Factory, TUIOS and libghostty; "verbal
    support and in-progress PRs" from **Claude Code**, cmux, Codex, Herdr, OpenCode and Pi. A
    Claude Code PR is the emitter amux would otherwise have to write as a hook — nothing has
    shipped in Claude Code yet.
  - **Rex server scalability** (alpha builds; Ubuntu 24.04, 16 CPUs, 64 GiB; two charts, read as
    images): **10,000 active terminals in 3.85 GiB** anonymous RSS (server + all terminals, dash
    shells, 1,000 numbered lines each), **1.16 GiB** once idle 60 s and snapshotted; 1,000
    terminals ≈ 0.4 / 0.14 GiB. Bash instead of dash adds ~423 KiB per shell. Shell-command round
    trip with 1,000 terminals open: median/p99 0.29/0.52 ms with none busy, 1.30/4.50 ms with 10
    busy, 1.58/15.24 ms with 100 busy (`date` every 0.1 s; machine CPU 0.2 / 1.6 / 13.6 %). Target
    is 100,000. Two mechanisms named: a C trampoline that unshares the fd table after fork (Linux
    sizes it to the highest open fd's power of two — 512 KiB per terminal beyond ~5,000), and PTY
    parking now triggering on **2 s** without read activity into a shared epoll thread (the 09-02
    devlog described parking idle-or-unobserved PTYs; this is the concrete threshold). No
    comparison against other multiplexers this time, deliberately.
- **2026-10-09** — status check: no new posts, videos or devlog since 10-07. Recorded here because
  the ledger had missed it: the **Rex user docs are public** at `superlogical.com/rex/docs` (the
  OSC 7501 spec is one page of them; the download itself is behind the invite login). They make
  the 09-14 CLI demo a documented surface:
  - **Self-describing server API.** `rex api list` / `describe` / `call`: named methods
    (`session.create`, `session.view`, `session.new_split`, `block.close`, `*.set_label`, …),
    each carrying a JSON Schema for input and output plus examples, and `--json` dumps the whole
    catalogue "for generating bindings". Blocks expose methods of their own
    (`com.superlogical.terminal.format` captures the screen, plus `.pwd` and `.title`). The CLI
    and Lua (`rex.call`, `rex.session.*`, `rex.on`/`rex.wait`) are both derived from the
    connected server's catalogue. Every id (`session:<uuid>`, `block:<uuid>`) is stable and never
    reused; labels are mutable. Every terminal gets `REX_SESSION`/`REX_BLOCK`/`REX_SERVER` in its
    environment, and `-S <host>` points a command at a remote server.
  - **A published event list.** Session events (`window_*`, `block_created`/`block_closed`,
    `client_connected`/`disconnected`/`connection_changed`, `session_view_changed`); terminal
    events (`child_exited` with exit code, `pwd_changed`, `title_changed`, `bell`,
    `process_changed` with the foreground process, `desktop_notification`, `progress_report`,
    `clipboard_written`, `program_status_changed`/`removed`); server events (`session_created`/
    `destroyed`, `client_joined`/`left`). `rex events --json` streams them as JSON lines.
  - **Scripting contract.** `rex run|split --wait`, `rex wait --for <dur> <block>`, `--keep-open`,
    and fixed exit codes (3 = target not found or ambiguous, 4 = server unreachable).
  - What's still missing: the client↔server wire protocol, code and a license. The API is RPC
    reached through the `rex` binary, `server.version` reports `0.1.0`, and a `method_not_found`
    error is documented as the expected result when a server is older than the docs.
  - **No third-party Rex client is announced.** Rex's clients are its own libghostty-based apps.
    The 07-29 post says Ghostty's "mission, governance, license, technical goals, and roadmap do
    not change". "Ghostty is the client" is Hacker News speculation, not a statement from
    Hashimoto. Asked on Mastodon (08-30) whether it would be part of Ghostty rather than a
    separate app, his whole answer was "Time will show". Nothing more is on record. The route into other
    terminals is libghostty: terminal-side pieces such as the binary snapshot are upstreamed
    there. Attaching from a terminal that doesn't speak the protocol is the architecture
    devlog's compatibility mode, which the docs don't cover.
  - **What libghostty has, as of this check:** the binary snapshot format (#13534, 07-31, marked
    experimental: "the format can and will change"; its stated uses include multiplexers),
    PTY-stream continuation tracking for replay (#13544, 08-01, "for me, this is used for
    multiplexers"), a snapshot-decode option to compress history (#14500, 10-01, C API
    `ghostty_snapshot_decoder_*`) and OSC 7501 (#14560). Those are the terminal-state halves a
    Rex client needs. The Rex wire protocol itself (attach, input, events) and any client code
    are not in the repo. Ghostty main also re-labels `ghostty.h` "libghostty-internal", tailored
    to the macOS app and "not designed for external use", and says external embedders should use
    libghostty-vt. warden embeds `ghostty.h`.
  - **Reception.** The July announcement drew 796 points and 406 comments on HN. Enthusiasm
    centred on Hashimoto's track record and the Ghostty non-profit. Skepticism centred on VC money
    for "a tmux clone", the investor list, and a pitch too vague to say what it builds. No Ghostty
    conflict-of-interest worry surfaced. The October beta posts drew no HN discussion at all; beta
    talk is on Discord, behind the invite. OSC 7501 got real debate (HN 141 points, Lobsters 35):
    - Windows Terminal maintainers split, from "I trust the folks at ghostty to be sane" to
      "smells like NIH".
    - Cargo will wait for adoption.
    - Critics called it AI-agent-specific, or argued for APC or systemd's OSC 3008 instead.
    - No kitty, WezTerm, foot or iTerm2 maintainer has responded.
    - Spec rev 0.3 (10-07): an absent `progress` now means indeterminate, and emitting without
      feature detection is allowed.

    Hashimoto (08-28): "we're not launching any hosted services". There's a Linux CLI server and
    a NixOS module, and Tailscale is built into the server.
- **2026-10-13** — status check: no devlog or video since 10-07 (the X posts are under 10-16). Two
  things landed:
  - **Claude Code emits OSC 7501**, shipped in v2.1.295 (10-08): "terminals that implement it can
    show whether Claude Code is working, waiting on you, or done". The hook amux would otherwise
    write exists upstream, but it only emits once the terminal answers `OSC 7501 ; ?` (its startup
    probe; no env var forces it on, `CLAUDE_CODE_DISABLE_TERMINAL_TITLE` turns it off). tmux 3.7c
    answers nothing (tested: a pane sending the query plus `CSI c` gets only the DA reply), so
    Claude Code under any tmux layer never emits. The Ghostty app drops `program_status` in its
    termio `StreamHandler`, query included, so a bare warden tab doesn't get it either.
  - **Spec rev 0.4** (10-10): the `?` query reply may now list the supported states and kinds, so a
    program can fall back from a later-revision state; terminfo `Pst` is removed. libghostty-vt
    follows in ghostty-org/ghostty#14630 (merged 10-10).
- **2026-10-15** — status check: no Superlogical devlog, YouTube upload or HN story since 10-07.
  Ghostty: no release since v1.3.1 (03-13). Since warden's vendored `b115e45`, `ghostty.h`
  gained only `ghostty_surface_paste_text` (paste protection for injected text; additive — warden's
  `cmd` injection deliberately stays on `ghostty_surface_text`), and snapshot decoding gained
  page-capacity validation (#14595). Still no `program_status` embedding action.
- **2026-10-16** — X posts from 10-11 to 10-15. None of them reached Mastodon. Still no devlog or
  video, and no Ghostty release. Nothing has touched `ghostty.h`, `embedded.zig` or the snapshot
  code since 10-15.
  - **Activity Mode** (10-11, @superlogical, quoted by Hashimoto): a Rex view of active and idling
    processes and agents, with metadata, for quick switching. It is built on OSC 7501.
    Hashimoto: "not as simple as 'program has activity lets show an attention icon.' Lots of
    heuristics and other signals as input to decide what exactly to show when." The design bar
    was an activity inbox the team would actually use instead of a noisy one. Testers were
    admitted in waves of 50–100 at that point.
  - **OSC 7501 adoption** (10-13): kitty implements it and shows it in the tab title, Grok Build
    shipped support, and Codex merged it. That makes Claude Code and Codex the two agents amux
    runs that now emit it.
  - **Rex iOS app** (10-12): the first build is in App Store review. iOS testing follows approval.
  - **Agents driving Rex across machines** (10-15): a tester's agent ran benchmarks on another
    person's Mac and brought both results back to compare. Superlogical plans evals so any
    harness can use Rex in few turns.
  - **Beta waves** (10-15): 250 invited the week of 10-05, about 2,000 the week of 10-12, and a
    1,000-person wave started on 10-15, in signup order. Almost 50,000 are still waiting, and
    wave size grows as stability holds.
  - **Ghostty main, 140 commits past warden's `b115e45`** (10-09). One embedding-API change in
    meaning: `OPEN_URL` kind `unknown` (regex-matched link text) is now documented as untrusted,
    like `osc8` (#14683, 10-13). The struct layout is unchanged. Ghostty.app has gated `osc8`
    since 08-05 and now gates `unknown` too: http/https/mailto with a host or address open,
    custom schemes ask first, and executable `file:` targets and control or bidi characters
    are refused. The policy is Swift in the macOS app, not libghostty; warden ports it in
    `surface/link.rs`. warden revendored to `b3dc3d7` (main, 10-15) the same day, which brings
    a fix for a use-after-free when scrollback
    prunes the cursor page (#14686), pty and terminal resizes synchronized (#14706), and
    CVDisplayLink start and stop moved to the main thread (#14691). The tmux control-mode viewer
    fixes and the libghostty-vt C API additions (prompt navigation, OSC 7501 rev 0.4,
    hierarchical context) don't reach `ghostty.h`.
- **2026-10-18** — status check: nothing since 10-16 beyond another 1,000 beta invites (X, 10-16).
  No devlog, video, HN story or new press; no Ghostty release; main is 11 commits past warden's
  `b3dc3d7` with none touching `ghostty.h`, `embedded.zig` or the snapshot code.
- **2026-10-19** — no Superlogical devlog, video, X post or HN story since 10-18. Ghostty main is
  18 commits past `b3dc3d7`, with none touching `ghostty.h`, `embedded.zig` or the snapshot code.
  This check added the sources above and found three things the ledger had missed:
  - **tmux declined OSC 7501** (tmux/tmux#5713, opened 10-09, closed 10-13). nicm: "This seems
    premature… between pane titles, OSC 9 progress bars and OSC 133 we have enough coverage of
    this kind of thing for the moment, let's see if this one takes off." tmux 3.8 (10-08) doesn't
    answer the query, so Claude Code still never emits under amux. A third-party patch onto tmux
    master (`autonomous-low-noise/tmux@4a77824`, 10-18) answers the query and keeps each pane's
    record as `pane_ps_*` formats. Nothing ships it.
  - **Ghostty is leaving GitHub** (mitchellh.com, 04-28). The destination is still in talks with
    "multiple providers (both commercial and FOSS)", the move will be incremental, and a read-only
    mirror stays at the current URL. As of today, PRs still merge on GitHub.
  - **The Rex docs have grown to 22 pages.** New since 10-09: a built-in agent skill for Claude Code
    and Codex (open tabs, split panes, read a screen, drive connected apps), and a read-only MCP
    server over the docs and the beta Discord's feedback, open to Discord members. There's still
    nothing on attach, the wire protocol or a license. The privacy page says Rex has no account or
    license checks. What the pages add for a warden client, beyond the 10-09 entry:
    - **Status can be pulled as well as streamed.** The terminal block has a `program_status`
      method that returns the current records. So a client that attaches late reads the state
      directly instead of replaying events. The other terminal methods are `format`, `process`,
      `pwd`, `title`, `size`, `write`, `reset`, `resize`, `set_theme` and `list_dir`.
    - **The `rex events --json` shape.** Each line is `{type, payload}`. A session event comes
      wrapped in `session_event` (adding `session_id`), and a terminal event is wrapped again in
      `block_event` (`block_id`, `creator_name`, `name`, `payload`). `program_status_changed`
      carries `id`, `record` and `reason`. Unknown events and fields are to be ignored, because new
      versions add them.
    - **Clients define the UI actions.** `pane.split`, `client.tab.*` and the rest come from the
      connected client (`rex -C <client> actions`), not from the server. A CLI or script can drive
      the Mac app only when Remote Control is on in the app's settings.
    - **Terminal options match warden's tab model.** They are `command`, `shell` (`none` execs the
      command directly), `cwd`, `initial_input` (text typed in at start, like warden's `cmd`) and
      `exit.on_completion`. Layouts are binary splits with a ratio. There are also floating layers,
      unplaced (detached) blocks, and server-side block plugins (`--block-dir`).
    - **Transport.** Local connections use a Unix socket. Remote ones use HTTP(S) on 443 or 7297,
      with callers identified only by Tailscale or the local peer uid. The docs call password and
      key auth **planned, "including support for existing SSH keys"**. That contradicts the 09-08
      demo's claim that SSH keys already work. The personal server starts on any `rex` command and
      exits about 10 s after its last session and connection close. Rex ships its own `xterm-rex`
      terminfo.
    - **The Linux server can't be installed yet, even by testers.** Its docs were written first.
      The 10-05 and 10-09 Linux mentions describe internal builds.
    - **Rex can configure Claude Code.** `rex skill setup` installs a `rex` skill in `~/.claude/skills`
      and adds a session-start hook to `~/.claude/settings.json`. The hook tells the agent its Rex
      session and has it name the session after its task.
    - **The release changelog is only in the beta Discord's #changelog**, which is readable through
      the community MCP. There's no public changelog page.
- **2026-10-24** — X posts only. There's still no devlog, video, new docs page or HN discussion,
  and the GitHub org has no repos. The attach and protocol searches of the docs still return
  nothing.
  - **Rex API compatibility across versions** (Hashimoto, 10-19, shipped in a preview release).
    Clients and servers of different versions now interoperate forwards and backwards. Each API
    change is recorded as a rule on a JSON pointer (RFC 6901). For each message, the rules between
    two versions compile lazily into a trie keyed by path tokens. A streaming JSON editor rewrites
    a message in one forward pass and copies untouched subtrees straight to the wire. Identical
    rule chains are interned, so a broadcast event is translated once per distinct chain rather
    than once per client. The API is at revision 4. A simulated 10,000-revision chain upgrades a
    20 KB body in about 35 µs. This was the gate on the Linux test program. For a warden client,
    it means a `rex events --json` consumer built against one server version keeps working
    against newer servers.
  - **The Linux server test program started** (10-21) with 100 people drawn from existing testers.
    The docs now say only members of a Linux testing wave can install it. 5,000 more joined the
    macOS beta the same day, about 7,500 that week, or roughly 15 % of the waitlist so far.
  - **The Rex iOS app is in external testing** (10-22), with dedicated iPad and iPhone Duo support.
    It will be free on the App Store.
  - **Kitty graphics now work over remote Rex sessions** (10-23), including image files that live
    on the remote host. Rex intercepts the file-based Kitty commands and resends the data to remote
    clients as in-band chunks, at under 1 % IO cost to everything else. The upstream half is in
    libghostty: the binary snapshot format now carries Kitty images, placements and pixels
    (ghostty-org/ghostty#14801, 10-21), restoring the layout before READY and the pixel data after
    it. #14776 and #14780 add a Kitty animation tick to the libghostty-vt C API and resolve Kitty
    placeholders in the render state. warden's surface already renders Kitty graphics, so none of
    this changes warden's own image path.
  - **Ghostty main moved to Zig 0.17** (#14519, 10-20; `minimum_zig_version = "0.17.0"`). Main is
    148 commits past warden's `b3dc3d7`. `ghostty.h` is unchanged. `embedded.zig` changed only
    through the treewide Zig 0.17 renames (`dupeZ` → `dupeSentinel`, `@intFromEnum` →
    `@backingInt`, and similar), so its behaviour is the same. warden revendored to `8ef0ec1`
    (main, 10-24) the same day, built on Zig 0.17.0. There's still no
    `program_status` embedding action. On the VT side, #14793 rejects OSC 7501 reports with
    malformed ids. Still no Ghostty release since v1.3.1.
  - **OSC 7501 adoption:** Rune 1.3 (10-23, HN 50 points) lists OSC 7501 support. Nothing new
    from tmux (#5713 stays closed, and 3.8 is still the latest tag), Claude Code or Codex.
- **2026-10-28** — X posts and a Ghostty release. There's still no devlog, YouTube upload or GitHub
  repo. The attach and protocol searches of the docs still find nothing about attaching.
  - **Ghostty v1.4.0 shipped** (10-27, tag commit `83fb956`; HN 600 points). It's the first release
    since v1.3.1. v1.4.0 is 61 commits past warden's vendored `8ef0ec1`, and main is 68 past it.
    `ghostty.h` and `embedded.zig` are unchanged in both. Only the snapshot code moved
    (`src/terminal/snapshot/screen.zig`, `terminal.zig`). Hashimoto's highlights: Kitty graphics
    fully conformant, animation included; scrollback compression (~90 % on average) and 50–90 %
    less memory overall; the Kitty clipboard protocol; macOS 27 support; `ghostty +ssh` installs
    terminfo on remote hosts. The release came with **13 security advisories**, published 10-27.
    Some of them are fixed only after warden's `8ef0ec1`, and those reach warden:
    - GHSA-w9q4-5jvv-mcv4: memory corruption when a screen clear prunes the cursor page (`e257bb4`).
    - GHSA-jqr7-69mm-v35h: a crash when narrowing a very wide page (`0af5c9e`).
    - GHSA-hp9c-qp4q-cpw3: a crash from zero-sized panes in a tmux control-mode layout (`bf14ea1`).
      Any program that writes to the terminal can trigger it, and the crash takes down the whole
      process.
    - GHSA-2r63-vmw6-3m3c: the zsh shell integration writes OSC 7 with `print`, so a crafted
      directory name can inject any escape sequence (an OSC 52 clipboard write, for example)
      (`ebd4499`). warden ships that script in `vendor/resources/ghostty/shell-integration/`.

    The use-after-free (GHSA-qjfx) and Kitty clipboard (GHSA-3r5j) fixes are already in
    `8ef0ec1`. The macOS link, OSC 8, drag-and-drop and file-open advisories are against the
    Ghostty app's Swift code, so they don't apply to warden. Its own link screening is in
    `surface/link.rs`. warden revendored to v1.4.0 (`83fb956`) the same day. Main's 7 commits past
    the tag carry a fix for a config-parser panic on truncated UTF-8 (#14879). warden loads the
    user's Ghostty config files, so it's exposed to that panic until 1.4.1.
  - **libghostty's status** (Hashimoto, 10-27). It now builds and runs CI for Apple platforms,
    Windows, Linux, FreeBSD, Android, WebAssembly and freestanding targets. It has bindings in most
    languages. Its users include Herdr, cmux, the Ghostel Emacs terminal and Antithesis' fuzzing,
    and Neovim has verbally accepted it as its terminal core (neovim/neovim#39773). **libghostty 1.0,
    with a declared ABI stability, is planned "in the next few months".** The post doesn't say
    whether that stability covers `ghostty.h`, which main labels libghostty-internal. Every example
    in it embeds the VT core.
  - **JetBrains IDEs run on libghostty from 2026.3** (10-27). It replaces JediTerm. libghostty does
    the parsing and holds terminal state, and the IDE does its own rendering. That makes it another
    libghostty-vt embedder, which doesn't change `ghostty.h`.
  - **Rex docs: 24 pages.** FreeBSD server install is documented next to Linux, and both are still
    limited to Linux testing waves. The Rex UI tour (Alasdair Monk, X, 10-27, a 6½-minute video,
    not transcribed) covers pane management, process and agent activity, themes and animation. An
    iPhone Duo redesign is due for testers the week of 10-26.
  - Nothing new from tmux (#5713 stays closed, 3.8 is the latest tag), Claude Code or Codex on OSC
    7501, and no new press.
  - **warden's maintainer is in the macOS test program**, which grants the beta Discord. Its #info
    pin (Hashimoto, 09-09, so the Discord predates the 10-05 public wave) is the test program's
    terms. Testers may say they're testing and share screenshots. They may not share access or
    invite anyone. Testing covers the macOS app only, and it self-hosts a Rex server and connects
    to other Rex servers. The Linux server is "Server only", and #alpha-registration enrolls
    testers for other platforms as they open. **Don't host a Rex server publicly or without
    Tailscale**, because "there are significant security risks in doing so at this time". The
    default is local connections only, and Tailscale is the only supported way to expose a
    server. The short-term priorities are the iOS client, **Rex as a full SSH replacement on
    Linux**, and remote-host file copies, port forwarding and socket sharing. Feedback goes in
    #feedback-mac, and accepted items move to #backlog. The pin also gives the community MCP
    setup: `claude mcp add -s user --transport http superlogical
    https://www.superlogical.com/rex/community-mcp`.
  - **First read of the community MCP** (#changelog and #feedback, through 10-27). Releases are
    split by platform: Mac `0.1.0-preview.22`, Linux Server `preview.9`, plus separate iOS and CLI
    builds. Several ship a week. A tester reported API revision 5 on 10-22, one revision past the
    10-19 post. What bears on the unlock:
    - **The client protocol is published by the server itself.** Each server serves its whole
      client spec as `GET /llms.txt` (and `/llms.html`, plus the method catalogue as JSON at
      `/api/help`), written for its own API revision (7 on the 10-27 Mac build), over its local
      socket (`~/Library/Application Support/rex/server.sock`, from `rex whoami`). The docs site
      leaves it out. In summary:
      - **Control:** a WebSocket at `/api/control`, subprotocol `rex.control.v1`, with one JSON
        object per text message. The client sends `hello` (with `api_revision`), and the server
        replies `welcome` (carrying a `reconnect_token`). Then come `request`/`response` pairs
        matched by `id`, and `event`s numbered by `seq`. A response's `seq` says which events its
        result already includes. A dropped client keeps its attachments for 60 s and resumes with
        its token.
      - **Data:** `data.connect` returns a single-use 60 s ticket for one block's `terminal`
        source. It needs a connection attached with `session.attach` or `session.bootstrap`. The
        client opens a WebSocket at `/api/data` with subprotocol `rex.data.terminal.v1` and sends
        a 17-byte binary HELLO (`0x01` + ticket). The server answers with a 32-byte WELCOME (a
        reconnect token, a resume window and a channel nonce) or a REJECT. After that, the stream
        is a **Ghostty snapshot**, then the program's raw output. Rex's own messages sit in it as
        `ESC _ rex1;<nonce>;<json> ESC \` (`resize`, `theme`, `child_exited`). A RESUME continues
        a dropped stream from an exact byte count within 60 s.
      - **Input** goes over control: `com.superlogical.terminal.write` (text and escape
        sequences) or `client.key`. Size goes through `terminal.resize` and follows the block's
        owner (`session.own_block`).
    - **A tester's Emacs front end attaches through it.** The tester is building Roost, which today
      shows tmux sessions through `tmux -C`. Their blocker was the snapshot: Emacs's terminal can't
      decode it, and `format` with `vt` isn't tied to a position in the stream. They asked for the
      opening screen as VT and for a stdio attach, and the request was deferred on 10-23.
    - **A throwaway relay proved the route for warden** (10-28). It's a small Rust program run as
      a pane's command. It attaches over control, takes `data.connect`, writes the stream to its
      own PTY and relays stdin through `terminal.write`. SIGWINCH goes to `terminal.resize`. For the
      opening screen it uses `format` `vt` and drops the snapshot bytes before the first Rex APC.
      Driven in a pseudo-terminal against the local server, output streamed, typed commands ran,
      a resize reached the server, and a fresh relay reopened on the previous screen. The gaps it
      left on purpose: decoding the snapshot instead of using `format` (a small race),
      `client.key` instead of raw text input, and RESUME.
    - **`rex attach` is gone.** It was the text-mode client, attaching inside the calling shell,
      which is the compatibility mode. It was removed in `preview.18` (10-21) "because it was
      incomplete and confusing people". Hashimoto: "Rex attach is gone for now."
    - **Driving the GUI client from outside works through Lua.** With Remote Control on, `rex do
      -e 'rex.client.queue("session.select", {session_id = …})'` moves the Mac app to a session.
      The client keeps its own focus state, and the server's focus state is advisory.
    - **Agent orchestration is meant for Lua.** An orchestrator wanted harness integration
      (readiness, prompt submission, conversation ids). Hashimoto said it "can and should be
      solved using our Lua scripting". There's no Lua method to report program status on a
      block's behalf yet. Status comes only from the block's own OSC 7501.
    - **OSC 7501 rough edges**, from testers running Codex, omp and Claude Code. OSC 133 prompt
      marks from omp drop its `working` record. Idle Codex returns no `program_status` record on a
      24-hour clock. Both are deferred. Rex shows status from remote tmux sessions only when the
      agent runs locally.
    - **Rex sessions don't survive their server.** A server restart (the in-app update asks for
      one), a crash, or a reboot ends every program in every session. A Linux server panic took
      every terminal down even though systemd restarted it in 0.1 s. Rex.app reopens on "No
      Sessions". There's no built-in restore of layout or processes. The requests for it
      ("Persist/resurrect sessions", 10-08; "Layout restore") are deferred. A team member (golden)
      said "we want to encourage use of Lua/scripting/APIs to do this", and that they aim for
      server restarts to become rare, with compatibility promises and no forced upgrades. So
      surviving restarts is up to the client, which is what amux's session ledger does for tmux
      today.
- **2026-11-06** — X posts and the community MCP. There's still no devlog (Hashimoto promises a
  formal update "next week"), YouTube upload, GitHub repo or new press.
  - **Rex sessions survive server restarts** (Mac `preview.23` and Linux Server `preview.11`,
    10-29). This falsifies the 10-28 "sessions don't survive their server". The server saves every
    session and restores it on start with its windows, splits, floating panes, names and **ids**.
    Each terminal comes back as a **new shell in its last directory**. Programs aren't restarted
    and scrollback isn't restored yet. `rex server run|start --restore=false` starts without them,
    and `rex server stop --discard` deletes them. `stop` now waits for the save. Restoring
    programs stays deferred. On "Upgrade/restart without killing agents" (deferred 10-31)
    Hashimoto said: "We have plans to do this but don't want to put it on the backlog yet." A
    request to have Rex remember each pane's agent conversation and resume it ("Restore Codex
    conversations after a server restart", 11-01, active) has testers bridging it with their own
    hooks and add-ons (`rexurrect`, herdr's agent session restore). For agent tabs, this is the
    same shape warden's restart handling already assumes (`docs/rex.md`, the restart row): same
    ids, bare shells, and warden types `cmd` and wakes agents by resume.
  - **The docs now document `data.connect`** in the Server API Methods page (30 pages, up from
    24). They also add Linux shared and login server modes, remote access, upgrading, Mac and iOS
    action and settings references, and built-in Tailscale. Linux and FreeBSD servers are open
    to every tester. The wire framing (`/api/control`, `/api/data`, HELLO and WELCOME) is still
    only in each server's own `/llms.txt`.
  - **CLI** (11-05): `rex whoami --json` reports `api_revision`, `api_min_revision` and
    `log_path`, and before this it always reported revision 0. `--interactive` on `run`, `split`,
    `new` and the `* new` commands makes the shell read `~/.zshrc`. New terminals take the current
    login shell as `SHELL`. The Linux server gained `--tailscale-control-url` for Headscale (11-02).
  - **Rex for Windows** (Alasdair Monk and Hashimoto, 11-05): a pre-alpha native client that is
    also a server, about two weeks from its first alpha wave. The Linux client is still to come.
    Floating panes demo (Hashimoto, 10-30). Beta rollout (10-28): 12,000 more invited that week,
    iOS testing open to every invitee, about 2,000 server testers, and just under half the
    waitlist invited.
  - **Ghostty main is 224 commits past warden's `83fb956`** (v1.4.0). There's still no release
    after v1.4.0, and 1.4.1 is promised "soon" with most of the throughput work backported.
    `ghostty.h` changed twice. #15039 (11-02) adds clang nullability annotations under `#pragma
    clang assume_nonnull`. That changes no ABI, and it only touches `embedded.zig` as a comment.
    #14208 (11-05) **appends** `GHOSTTY_ACTION_SET_WINDOW_THEME` at the end of
    `ghostty_action_tag_e` and its union member, so existing tags keep their values. warden's
    tags are bindgen-generated anyway (`build.rs`). The snapshot code changed only through
    style packing and hyperlink-map sizing. There's still no `program_status` embedding action.
    The libghostty-vt work (#15069 to #15098) prepares tagged, ABI-checked libghostty-vt releases
    with versioned exports. That's the 1.0 ABI-stability plan from 10-27, and it's about the VT
    library, not `ghostty.h`. IO throughput on main is 1.4–16× v1.4.0 depending on the workload
    (Hashimoto, 11-03). Termius mobile now runs on libghostty (10-30).
  - **OSC 7501:** tmux #5713 stays closed, and 3.8 is still the latest tag. nicm (10-30) finds it
    "useful for Claude and Codex" but the protocol "far too complicated and heavyweight", with
    64 records per pane as the sticking point. A narrow patch that keeps one record per pane
    (cosgroveb, 11-03) is waiting for his review. New adopters: kitty 0.50.0 and the Antigravity
    CLI 1.4.0. Nothing new from Claude Code or Codex.
- **2026-11-07** — libghostty-vt 1.0 and the community MCP. There's still no devlog, YouTube upload,
  GitHub repo or new press, and the attach and protocol searches of the docs find nothing new.
  - **libghostty-vt 1.0 shipped** (11-06, tag `vt-1.0.0` at `0fa3d57`, 266 commits past warden's
    `83fb956`; `mitchellh.com/writing/libghostty-1-0`, HN 444 points). It has its own release cycle,
    separate from Ghostty's. 1.0 means semantic versioning and a stable C ABI. Objects are opaque
    handles read and written through enum-keyed getters and setters, a newer header against an older
    library gets `GHOSTTY_UNSUPPORTED`, the few public structs carry their size or reserved padding,
    and CI checks every change against a libabigail baseline (Linux) and golden type manifests
    (macOS, WebAssembly). Releases are a signed source tarball plus an offline dependencies tarball,
    and macOS gets a dylib and an XCFramework. It's MIT-licensed with no copyleft dependency. Main
    is now `1.1.0-dev` (#15128). The release covers **libghostty-vt only**. `ghostty.h` isn't part
    of it and stays libghostty-internal, with no `program_status` action.
    - **For warden-relay** (which vendors libghostty-vt built from `83fb956`, statically linked):
      every function it calls still exists in 1.0 (`ghostty_snapshot_decoder_*`,
      `ghostty_snapshot_encode_alloc`, `ghostty_formatter_*`, `ghostty_terminal_*`). The formatter
      options did change, though. The size-carrying `GhosttyFormatterScreenExtra`/`…TerminalExtra`
      bool structs became one `GhosttyFormatterExtras` `uint32_t` bitmask
      (`GHOSTTY_FORMATTER_EXTRA_*`). `crates/warden-relay/src/vt.rs` hand-declares the old structs
      (`ScreenExtra`, `TerminalExtra`), so revendoring onto `vt-1.0.0` means porting them to the
      bitmask, set per handle with `ghostty_formatter_set`. warden revendored to `vt-1.0.0` the same
      day, with `vt.rs` ported. simdutf and Highway now build without libc++, so the relay no
      longer links it. Within `vt-1.x` the relay's hand-written declarations hold.
    - Adopters named in the post include KDE (a Konsole replacement experiment), Turborepo, devenv,
      OrbStack and ghostty-web. Neovim's libghostty-vt PR was waiting only on a tagged release.
  - **Rex** (Mac `preview.26` 11-05, `preview.27` 11-06, CLI 11-06; nothing on the wire protocol,
    events or API revision). The experimental Go to Anything / View Activity switcher (`preview.26`)
    shows every session, tab and pane in one list, each with its program's state. It's the Activity
    Mode from 10-16, shipping. The Done notification now skips an agent cancelled with Escape, one
    pausing between turns, and one that exits with a sub-agent still running. That's attention policy
    of the kind warden's `rex/attention.rs` has to make for itself. `preview.27` adds file upload to
    remote terminals (capped at 2 GB per terminal on the server, oldest uploads evicted first) and
    Handoff between Mac and iPhone. It also fixes server crashes when many terminals restore at once
    on macOS. Alasdair Monk points to `neorex` (11-01, `github.com/alexrlocke/neorex`), a community
    Neovim plugin that gives Neovim Rex-native splits and floating windows. It's a third-party Rex
    integration, built on the API.
  - Nothing new from tmux (#5713 stays closed, 3.8 is the latest tag), Claude Code or Codex on OSC
    7501. Ghostty has no app release after v1.4.0. Since 11-06, nothing has touched `ghostty.h`,
    `embedded.zig` or the snapshot code.
- **2026-11-09** — X posts and iOS builds. There's still no devlog (the "next week" update hasn't
  landed), YouTube upload, GitHub repo, new docs page (30) or press.
  - **The Rex Data protocol** (Alasdair Monk, 11-07): files drag-and-drop and copy-paste between
    devices, "built on top of our new Rex Data protocol", with no ssh, rsync or ftp. iOS builds
    1868/1870 (11-07) paste an image into a terminal by uploading it to the terminal's host and
    typing its path, as the Mac does. The docs don't mention it. It reads as the `/api/data`
    channel gaining sources beyond `terminal`. That's additive for warden: `rex-client` opens only
    `rex.data.terminal.v1` (`DATA_PROTOCOL`).
  - **"@linear 🤝 @superlogical"** (Alasdair Monk, 11-08): the post is one image, which this check
    couldn't fetch. Nothing else says what the tie-up is.
  - Ghostty main is 33 commits past `0fa3d57` (`vt-1.0.0`). Of the watched files only
    `include/ghostty/vt/version.h` changed (1.1.0-dev). Two resize fixes reach warden on the next
    revendor: dangling pins on column growth (#15134) and scrollback not pulled down when rows grow
    as columns shrink (#15141). libghostty-vt now publishes Windows zips (#15151). Libghostty 1.0
    reached 1,069 points on HN. Nothing new from tmux (3.8, #5713 closed), Claude Code or Codex on
    OSC 7501.
- **2026-11-10** — the community MCP and X posts. There's still no devlog, YouTube upload, GitHub
  repo, new docs page (30) or press, and the Linear tie-up is still only an image.
  - **Rex connects over SSH** (Linux Server `preview.14`, 11-09). `-S ssh://[user@]host` on any CLI
    command reaches a server through the user's own ssh and ssh config, and starts a personal
    server there if none is running. The changelog pitches it as replacing Tailscale for many users.
    `rex server install ssh://host` installs Rex on a systemd Linux host over ssh, starts it at
    boot and adds it to the hosts. The Remote Access docs also describe forwarding the server's
    Unix socket over ssh and connecting to the local end. Other changes: `install.sh --user`
    installs without root, `rex server status` lists every listener, and stopping the service now
    saves sessions before their shells exit on SIGTERM. For warden this is additive. `rex::server`'s
    `host_endpoint` reads `unix:`, `https://` and bare `host[:port]`, and skips `ssh://`. So a host
    reachable only over ssh fails to open with "no endpoint warden can reach". A forwarded socket
    already works as a `unix:` endpoint.
  - **The Mac client overrides API split ratios** (feedback, 11-08, active). `session.new_split`
    and `session.move_block` honour `ratio`, then an attached Mac app rebalances the split to 0.5
    (`Workspace.balancesSplitsOnCreation`). A session with no Mac client attached keeps the ratio.
    This doesn't affect warden's own splits: `rex/column.rs` always asks for 0.5 and sets other
    ratios through `session.resize_pane`, which the report gives as the workaround.
  - OSC 7501: Paneflow (an agent orchestrator) moved from screen rules to OSC 7501 (blog, 11-03).
    A community project, `rexmod` (11-01), turns Rex into an agent harness. Nothing new from tmux
    (3.8, #5713 closed), Claude Code or Codex.
  - Ghostty main is 68 commits past `0fa3d57`. Of the watched files, only
    `src/terminal/snapshot/page.zig` changed, and only through CI test sharding. #15173 keeps a
    pruned page's memory when grow prunes by lines. There's still no 1.4.1.
- **2026-11-13** — tmux takes OSC 7501, libghostty-vt 1.0.1, and Rex releases. There's still no
  devlog (the "next week" update promised 10-28 hasn't landed), YouTube upload, GitHub repo or
  press. The docs have 32 pages; the two new ones install and run the Windows server, which is open
  to testers with Discord's Windows role. The attach and protocol searches find nothing new.
  - **tmux master answers OSC 7501** (`434ab3a25`, 11-12, "Minimal support for OSC 7501 program
    status", from cosgroveb's tmux/tmux#5830). nicm applied it with changes. tmux answers the `?`
    query and keeps **one root record per pane** (reports with an `id` are ignored), exposed as the
    `pane_program_state`, `pane_program_name` and `pane_program_message` formats and shown in tree
    and switch mode by default. `done` and `error` survive OSC 133 A and the pane exiting. It
    doesn't pass the report through to the outer terminal. It isn't in a tag yet (3.8 is the
    latest). Once it ships, Claude Code and Codex under tmux emit. Under amux that reaches tmux's
    formats, not warden, which reads status only from Rex.
  - **libghostty-vt 1.0.1** (11-11, tag `vt-1.0.1` at `e9dc690`, a drop-in on the 1.0 ABI). It adds
    prebuilt Windows builds and fixes: stale OSC 133 prompts after a column resize (#15199), the
    grow-rows-shrink-columns scrollback pull (#15141), failed render updates leaving rows marked
    clean, and a replaced Kitty image vanishing mid-transmission (#15184). A restored snapshot's last
    page now gets its full pooled capacity (#15195), with the snapshot format unchanged. A
    scrollback line limit is no longer slower than a byte limit. warden-relay vendors `vt-1.0.0`,
    so these reach it on the next revendor with no code change. Ghostty main is 98 commits past
    `0fa3d57`. Of the watched files, only `vt/version.h` and the snapshot decoder changed
    (#15195). `ghostty.h` and `embedded.zig` are untouched, and there's still no 1.4.1.
  - **Rex** (Mac `preview.28` 11-10 and `preview.29` 11-12; Server & CLI `preview.15` to `.18`,
    11-10 to 11-12). Nothing changes the wire protocol, the events or the API revision. Both the
    server and the Mac client moved to libghostty-vt 1.0.1. The parts that bear on warden:
    - **ssh hosts mature.** Several connections to one host share one ssh login. A busy host's
      dropped logins (sshd `MaxStartups`) are retried. The Mac app asks ssh's passphrase, password
      and host-key questions in its command palette. Terminals on a host that went down reopen when
      it comes back. warden's `host_endpoint` still skips `ssh://` (11-10 entry).
    - **New terminals report their pixel size from the start** (`preview.15`), so Kitty images and
      chafa get the right size before the first resize.
    - **A pane holding large images no longer fails** with "snapshot exceeded the 64 MiB limit"
      (Mac `preview.28`). That limit was the Mac client's.
    - Each terminal gets its own cgroup on Linux under systemd, so systemd-oomd kills one terminal
      rather than every session.
  - **Discord feedback** (community MCP, since 11-10):
    - **Clients can reach a server over plain HTTP** on the bleeding-edge channel (golden, 11-12,
      "Allow the server to be exposed over HTTP", deferred). The server has exposed an HTTP
      listener (`--extra-listen`) for some time, and only the client side is new. It serves
      Headscale setups that can't issue certificates. warden can't follow: `rex::server`'s
      `host_endpoint` takes `unix:`, `https://` and a bare `host[:port]` (as https), and rejects
      `http://`.
    - **Codex's async questions don't show as blocked** ("Show unanswered questions in Activity
      while agents keep working", 11-10, active). Codex 0.167 reports root `blocked:kind=question`
      only once the agent stops running, so a question it asks while it keeps working leaves the
      record at `working`. A Codex agent in a warden agent tab with a question pending reads as
      working, not as needing you, in `rex/attention.rs`'s terms too.
    - The third-party streaming thread ("Terminal streaming API: minimal client example", 10-28)
      still has no team answer. The split-ratio reports (11-08, 11-12 on iOS) are the 11-10 item.
  - **On X**: Alasdair Monk runs 25–30 agent sessions on several machines and harnesses from the
    macOS client (11-09). Hashimoto described how Superlogical works (11-10). Nothing new on the
    Linear tie-up.
  - Nothing new from Claude Code or Codex on OSC 7501.
- **2026-11-20** — Rex releases, a video and X posts. There's still no devlog (the update promised
  10-28 hasn't landed) or press, and nothing moves the unlock: no change to the wire protocol, the
  data stream or the event shapes. The docs have 33 pages (the new one is Custom UI, below), and the
  attach and protocol searches find nothing new.
  - **The API revision is moving fast.** A tester reports revision 13 on Server `preview.18` (11-12)
    and 16 on the snapshot channel (11-17), against 7 on 10-27. `rex-client` says hello at
    `API_REVISION` 7. The 10-24 compatibility layer translates between revisions both ways, so this
    holds as long as servers keep 7 at or above their `api_min_revision`, which nothing published
    states.
  - **Server & CLI** (`preview.19` 11-13 to `preview.22` 11-19). Additive for warden:
    - TCP port forwarding, `rex forward` (`-L`/`-R`, reconnecting). Terminal blocks gain a `ports`
      method and a `ports_changed` event (each listening port and its process). The Mac app
      forwards a remote `localhost` link on ⌘-click (`preview.31`). The 11-16 video
      (*Rex Demo: Remote Copy/Paste, Drag/Drop, and Port Forwarding*, YouTube `pkH_WJApzH0`, 7 min,
      not transcribed) shows this and the Lua plumbing under it. Hashimoto (X, 11-16): "the moment
      any UI shows a selector between 'local' and 'cloud', the product has failed."
    - A per-user **focus history** with an API method to step back through recent panes, tabs and
      sessions (`preview.22`). Closing the focused pane or tab now returns focus to the previous one.
    - Terminals restored while the server is slow now "come back with their contents and directory
      instead of as new shells" (`preview.20`). That reads as restored terminals carrying their
      screen contents, which the 11-06 entry said wasn't restored yet. Programs still aren't.
    - Stream fixes that touch warden's relay path: a data stream that drops and reconnects no longer
      sticks when the client stops reading partway through catching up or loses the reconnect reply
      (`preview.19`); a terminal that paused output for a client that stopped reading resumes when
      another client connects (`preview.21`); an over-large event no longer disconnects every client
      in the session (`preview.21`). Control requests can be up to 1 MiB (`preview.22`).
    - Closing a terminal kills programs that ignore SIGHUP, and stopping a server with many such
      terminals takes about one second in total.
  - **Mac** (`preview.30` to `.33`): copy mode (⇧⌘C), `rex.ui.input` / `rex.ui.pick` asking
    questions in the command palette (on every attached client, first answer wins; Hashimoto and
    Alasdair Monk, X, 11-18), every setting as an action callable by `rex do`, bottom tab bar,
    New Session on Host, a confirm before closing a background tab with unseen output, and the app
    renamed from Rex Beta to Rex. None of it is API warden reads.
  - **Rex for Windows alpha began** (11-17), a native client that is also a server. Windows releases
    are versioned `0.1.1-preview.N`.
  - **A Superlogical repo exists:** `superlogical/Sparkle`, a fork of the macOS update framework
    (created 11-19). It's not code for Rex's protocol or a license.
  - **Hashimoto has a new terminal specification proposal** (X, 11-19), shared privately with a
    group that includes people working on Claude Code. Nothing about its content is public.
  - **Discord feedback** (community MCP, since 11-13):
    - "Server restart drops the Terminal startup command and opens zsh" (11-13, active): restored
      terminals start the account shell, not the Mac app's startup command. warden doesn't hit it:
      its Rex blocks carry only `cwd` and `initial_input` (`rex/mod.rs`), so they run the account
      shell before and after a restart alike.
    - "Output takes 90 s to come back after a conn drop, even though the control conn resumes in
      seconds" (11-11, active): the Mac app's `data.connect` after a drop lags control by about
      90 s. Client-side so far; warden's relay reconnects its own data stream.
    - "Rex's Program Status Protocol Hold / Queued Status" (11-18, active) asks for a state between
      `working` and `blocked`. The 11-13 Codex async-question gap stays open.
  - **OSC 7501:** tmux/tmux#5889 (cosgroveb, 11-15, open) asks tmux to forward program status to the
    outer terminal. nicm has no plans but would take a PR that sets only the root record from the
    current pane; a contributor offered to write it. tmux 3.8 is still the latest tag. Claude Code's
    changelog since 10-08 adds fixes (background sessions, the trust prompt as blocked, background
    tasks listed). Nothing new from Codex.
  - **Ghostty:** no app release after v1.4.0. warden revendored both libraries to `vt-1.0.1`
    (`108cacb`). Main is 53 commits past it, and of the watched files only `vt/version.h` and
    `snapshot/page.zig` changed; `ghostty.h` and `embedded.zig` are untouched. Main moved to Zig
    0.17.1 in Nix (#15230).
- **2026-11-24** — tmux forwards OSC 7501, plus a snapshot decoder fix. There's no devlog, docs
  change (still 33 pages, attach and protocol searches unchanged), Mac or Server & CLI release
  since `preview.33` / `preview.22` (11-19), and nothing moves the unlock.
  - **tmux master forwards the active pane's OSC 7501 status to the outer terminal** (`9e5702fb`,
    11-23, PR #5907 from cosgroveb, closing #5889). tmux sends `OSC 7501 ; ?` to the outer terminal
    on attach and forwards only if it answers, or if the `programstatus` terminal feature is set; it
    clears the status on detach. tmux 3.8 is still the latest tag. warden's plain tabs don't answer
    the query (no `ghostty.h` action; warden's `docs/FOLLOWUPS.md`), so nothing reaches warden yet.
    With that action and the reply, a plain tab running tmux would get the active pane's status.
  - **Ghostty:** #15328 (11-23): a restored snapshot keeps the cursor's pending wrap at any column,
    not only the last one (left/right margins, DECSC/DECRC). Decode-only; the snapshot format is
    unchanged. warden-relay decodes Rex snapshots with libghostty-vt, so warden revendored both
    libraries to main `2474129` (`196f93f`), 95 commits past `108cacb`. Of the watched files only
    `snapshot/screen.zig` and `snapshot/snapshot.zig` changed (this fix), and `ghostty.h` and
    `embedded.zig` are untouched. No app release after v1.4.0.
  - **`rex-client` now says hello at `API_REVISION` 14** (warden `0e3759d`), so the 11-20 concern
    about servers dropping revision 7 no longer applies.
  - **ssh install is shipped:** `rex server install ssh://<address>` installs and starts a server
    over SSH, and every client can use it (Hashimoto, X, 11-19). warden's `host_endpoint` still
    skips `ssh://`.
  - **Rex Windows** `preview.33` to `.35` (11-20 to 11-23), embedding server `preview.22`. Nothing
    warden reads.
  - **Discord feedback** (community MCP, since 11-20): "Program status: let API clients know if a
    done/error record has been seen" (active) asks Rex to expose whether a client has seen a
    `done`/`error` record. warden keeps that state itself (`rex/attention.rs`). "session.recent does
    nothing" (11-20, active) is the `preview.22` focus history, which warden doesn't call.
  - Nothing from Claude Code or Codex on OSC 7501, and no press (the 11-23 Hacker News item reposts
    the 10-05 devlog).
- **2026-11-28**: Rex resumes programs across a server restart through a new open protocol, OSC
  7577. That overlaps warden's own resume report. There's no devlog post. The docs have 36 pages,
  and the new ones are Program Resume Protocol, Resume Rules and the Windows and FreeBSD server
  pages. The attach and protocol searches find nothing new about the data stream.
  - **OSC 7577, the Program Resume Protocol** (`/rex/docs/build/program-resume`, revision 0.5,
    11-25; Hashimoto, X, 11-25: written with Herdr and reviewed by other terminal and program
    authors). A program reports how to bring it back, and after a restart the terminal may run that
    command:
    - **Report.** `OSC 7577 ; pid=<owner>:time=<epoch s>:argv=<base64>,<base64>…[:cwd=<base64>]:mac=<hex> ST`,
      or `clear=1` in place of `argv`. `mac` is SHA-256 of the body before `:mac=`, then `:`, then
      the token. It's a plain hash, not an HMAC, so a shell can compute it with the token on stdin.
      The terminal ignores a report whose `mac` doesn't match or whose `time` is more than about
      60 s from its clock. Unknown keys are ignored unless `need` names them.
    - **Detection.** The terminal sets a random `OSC7577_TOKEN` (22–64 chars) in the environment
      of each pty's first process, new on every start and never shared between panes. There's no
      query. A program that reports removes the token from its children, and one that runs children
      with fewer rights (a sandbox) removes it from them too. Never forward it over ssh.
    - **Records.** The `pid` owns the record (it validates, but isn't security). The terminal clears
      the record when the owner exits on its own, keeps it when the terminal itself ends, and
      prefers an ancestor's record over a descendant's.
    - **Restore.** The terminal runs `argv` as separate arguments inside the user's shell after its
      startup files, never as shell source, as the same user, in `cwd` if one was given (it doesn't
      resume if that directory is gone), and never retries a failed resume. No environment is
      carried over. A missed limit or a changed command should send `clear` first.
  - **Rex's side** (Server `preview.25`, 11-25; the Mac app embeds it from `preview.35`). On a server
    restart Rex resumes a program that reports and is trusted by a rule, or one a `detect` rule
    matches by its command line. Every other terminal is a shell in its saved directory, as
    before. Trust rules live in `init.lua` under `resume.trust.<name>`: a `command` pattern
    (`rex.match.uuid` / `id` / `name` / `under`), `flags` set to keep, drop or refuse, `dir` and
    `except`. Built-in rules cover `claude --resume <uuid>`, `codex resume <uuid>`, copilot, crush,
    gemini, opencode and pi; `resume.defaults = false` turns them off. Rex refuses a program that
    runs under anything but a shell (`wrapped`) and never tries the same record twice. **Claude Code
    reports through the session-start hook that `rex skill setup` installs**, not by itself. Pi
    reports natively after 1.6.1, and nothing else reports yet. The API is the `program_resume_records`
    block method (records with `command`, `chosen`, `trusted`, `rule`, `untrusted_reason`) and a
    `terminal.program_resume_changed` event, whose records leave the command out because it may
    hold a secret. Saved commands sit in plain text in the server's state directory.
  - **Bearing on warden.** warden carries its own version of this: `claude-hook.sh` writes
    `ESC _ warden1;resume=<base64> ESC \`, the relay forwards it, and `agents::RESUME_SHAPES`
    allowlists it (warden's `docs/rex.md`). Rex now holds the same record, with an allowlist the
    user configures and broader harness coverage, readable over the control connection warden
    already has. warden doesn't read it yet. This machine has only warden's hook (no `rex skill
    setup` hook), so Rex resumes no Claude Code here, and a Rex-resumed program in a restored
    window would meet `retire_restored`, which sleeps a resumable agent and closes its window.
  - **Discord feedback on resume** (community MCP, 11-25 on). Resume fails when zsh startup files
    toggle job control (SIGTTOU, active; one report deferred), when `CLAUDE_CONFIG_DIR` picks the
    Claude home (no flag can carry it, deferred and active), when a launcher wraps `claude`
    (`wrapped`, active), with direnv (accepted), when the login shell is a wrapper script, and for
    tcsh. The saved command keeps the launch flags even after `/model` changes them mid-session
    (active). `CLAUDE_CONFIG_DIR` matters on this machine, which runs Claude Code from per-profile
    homes.
  - **Server & CLI `preview.23` (11-23), which the 11-24 entry missed:** new terminals no longer
    set `REX_SESSION`, and `REX_BLOCK` identifies the session wherever `-s` goes. warden's hook
    keys on `REX_BLOCK` already. `ssh://` endpoints gain IPv6, paths with spaces and Mac hosts
    found without naming a socket. Focus is reported only when it changes, and mouse motion only on
    a cell change.
  - **Server & CLI `preview.24` (11-25):** `client.undo_close` restores what a client closed in the
    last minute or so. `session.transfer_window` / `rex window move --to` moves a window to another
    session with its programs and scrollback, which is the accepted "Move a pane to a new session"
    request. Also `rex cp`, kitty images compressed in transit, DNS-rebinding refusal on plain-HTTP
    listeners, and an HTML capture that keeps only http, https, mailto and file links. warden calls
    none of these. The Mac app (`preview.34` to `.37`), iOS 0.1.1 (now in the App Store; Hashimoto,
    X, 11-26) and Windows (`preview.37` to `.42`) changes are client UI only.
  - **Ghostty:** `vt-1.0.2` (11-24; Hashimoto, X) is 11 commits past warden's `2474129`: skin-tone
    emoji widths, IL/DL wrap and prompt-mark fixes, faster scrolling in left/right margins, Zig
    minimum lowered to 0.17.0. Main is 45 commits past `2474129`. Of the watched files only
    `vt/unicode.h` and `vt/version.h` changed, and `ghostty.h`, `embedded.zig` and `snapshot/` are
    untouched. No app release after v1.4.0.
  - **Hacker News:** "Program Resume Protocol (OSC 7577)" (11-25) and "Ask HN: What do you all think
    of Rex" (11-26). No press.
  - Nothing new on OSC 7501 from tmux, Claude Code or Codex. Codex is on `0.174.0-alpha.9`.

## What this changes here

**The destination is warden as the client and Rex as the session backend, with amux
retired.** The measure of progress is the number of terminal emulators between the host
client and each PTY. Today a framed agent has two (frame + agent) and an unframed one has
one. Rex's native path has zero. tmux leaves when Rex takes over sessions. amux leaves too,
because everything it does is either something Rex does natively or something warden owns
as the client.

- **The frame layer stops being warden's path.** warden already embeds one libghostty
  surface per PTY behind its `TerminalSurface` seam, which is the split model Superlogical
  describes, so warden's native splits replace the frame's fixed two-pane layout. Every
  invariant of the form *"re-assert this terminal feature on the frame socket too"*
  (`extkeys`, `sync`, `hyperlinks`, `allow-passthrough`, the per-layer OSC 777 wrap counted
  by `_tmux_nest_depth`, `bin/amux`'s re-assertion on every `--frame`) stops applying to
  that path.
- **amux is the bridge until Rex can host warden's sessions, and gets no new
  architecture.** No backend seam and no second session backend: those only paid off if
  amux outlived tmux. Fixes and small features to the tmux path stay worthwhile because it
  is what runs today. `--frame` keeps serving standalone terminals and remote hosts until
  amux retires.
- **Where each amux capability goes when amux retires:**

  | amux today | After |
  | --- | --- |
  | sessions that outlive the client | Rex server-side sessions (persistent, parked when idle) |
  | the session ledger, restore after a server restart or reboot, sleeping a server | warden: each agent tab keeps its conversation id, a server restart brings tabs back asleep, wake resumes them; sleeping a server sleeps its tabs, then stops it (Rex restores sessions and layout across its own restart, and resumes only a program that reports through OSC 7577 and that a trust rule allows; warden's hook doesn't report, so its agents come back as bare shells) |
  | `amux @host` over ssh/et/mosh | Rex servers on each host, reached with `-S <host>` / the client's remote sessions |
  | agent state in tab names, OSC 777 attention | OSC 7501 from the agent itself, read from Rex's `program_status_changed` events |
  | presence probe (`amux --probe`) | Rex `session_created`/`destroyed` and `child_exited` events |
  | directory-based agent selection, per-agent launch commands | warden's per-tab `cmd` cascade |
  | AI summary status lines, the note row | warden's amux-retirement project, under the selected agent tab |
  | the `[frame]` terminal-pane layout (`left_vertical_split`, `left_top_panes`) | warden mirroring Rex split layouts, declared per tab and created as Rex splits |

  The lock-in point lives here too. Rex's client and server are free, self-hosted,
  account-free and not directly monetized (2026-10-05), so the venture-funded risk is the
  unannounced commercial layer. warden reads Rex through its documented API and events,
  which keeps that layer optional.
- **Agent status reaches warden from Rex, not from the terminal.** Claude Code emits OSC
  7501 only to a terminal that answers its query (ledger, 2026-10-13). Rex answers it. The
  Ghostty app doesn't. tmux master answers it (2026-11-13) and forwards the active pane's status
  only to an outer terminal that answers tmux's own query (2026-11-24), which warden's surfaces
  don't. So under amux nothing reaches warden, and amux doesn't grow a
  7501 bridge for a layer that is going away. Under Rex, warden reads the events on its control
  connection and renders the states as chrome, which is where warden's sidebar and tab-row dots already
  live. The events are raw states. Deciding what deserves attention takes a policy on top,
  which Rex's own Activity Mode builds from heuristics (ledger, 2026-10-16), and warden would
  have to write its own. The `ghostty.h` action matters only for plain tabs outside Rex (warden's
  `docs/FOLLOWUPS.md`).

**warden is a Rex client, through a relay** (built on warden's `dev`, 2026-10-28; architecture in
[`rex.md`](rex.md)). Rex's published client
protocol (ledger, 2026-10-28) streams a block as a snapshot, then raw output. `ghostty.h` can neither load a snapshot into a
surface nor feed one from anything but its own PTY, so each Rex pane's command is a relay
subprocess: it writes the stream to the surface's PTY, decoding the opening snapshot with
libghostty-vt, and sends input back over control. The relay beat patching libghostty because a
patch would sit in Ghostty internals and carry a fork through every revendor, security ones
included. warden speaks the control protocol itself; the `rex` CLI isn't on its path. A pane shows
one block. Rex restores sessions across its own restart and resumes programs that report through
OSC 7577 (ledger, 2026-11-28), but warden keeps its own resume record (its hook's report plus
`agents::RESUME_SHAPES`), so an agent's conversation survives through warden's agent tab list
(sleep/wake, conversation resume). Reading Rex's `program_resume_records` instead waits on Rex
resuming a Claude Code that runs from a per-profile home ([`FOLLOWUPS.md`](FOLLOWUPS.md)).

