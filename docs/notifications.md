# Notifications — design, the macOS-26 banner gap, and how to diagnose it

warden surfaces a terminal's attention signals (bell, OSC 9 / OSC 777 desktop notification) on the
tab that raised them: an **amber badge** on the tab row, and — for a desktop notification on a tab
that isn't the visible one — a **native macOS banner**. The path is `libghostty action_cb` → a
seam-neutral `SurfaceEvent` → `notify.rs::handle` → badge (`warden:notify`) + `show_banner`
(`UNUserNotificationCenter`). See the module docs in `crates/warden-app/src/notify.rs`.

## Footgun: a missing banner is almost never a warden bug

When a banner doesn't appear, the tempting move is to "fix" `notify.rs`. **Don't start there.** The
warden side of this path was verified end-to-end on a notarized `warden.app` (macOS 26):

- the OSC escape decodes to a `Notification` (not a bell — bells are badge-only **by design**),
- the originating tab is correctly identified as **not visible**,
- `show_banner` posts the request and **usernoted accepts it** (completion error is nil),
- the delegate's `willPresentNotification` **fires** and returns `.banner | .sound` (the value is
  correct — `Banner` is `1<<4`), which is what un-suppresses a banner while warden is frontmost.

At that point warden has done everything the `UNUserNotificationCenter` API requires. It was also
confirmed that `getNotificationSettings` reports the app **Authorized**, alert style **Banner**,
alerts **Enabled**, and that **no Focus/Do-Not-Disturb assertion** was active. The banner still did
not render. So the suppression is in macOS's notification *presentation* layer, downstream of
everything warden controls.

The `.banner | .sound` value itself is also correct — re-verified against the binding
(`objc2-user-notifications`): `Banner = 1<<4 = 16`, `Sound = 1<<1 = 2`, so the delegate returns
`18`, the exact NSUInteger that un-suppresses a foreground banner. There is no remaining lever on
warden's side: returning `.banner` from `willPresent` *is* the only mechanism an app has to present
a banner while it is frontmost.

This is the trap: every signal warden can read says "should show a banner," so the instinct is to
keep changing warden. The evidence says the lever is the OS, not the code.

### The regression has a foreground-only variant — mind the discriminator

The suppression is **not always unconditional.** On the author's machine it killed *every* banner
(foreground and background alike). On another affected machine it manifested **foreground-only**:
banners rendered normally when warden was **backgrounded** but were dropped only when a warden window
was **frontmost** — while the `notify_debug` trace still showed `willPresent fired -> returning
.banner | .sound`. This is fully consistent with the mechanism: macOS consults `willPresent` *only*
in the foreground, so a presentation-layer fault on that path suppresses foreground banners while the
background delivery path (which never touches `willPresent`) keeps working.

The practical consequence is that the naïve discriminator "do *any* banners appear?" can mislead —
warden's own backgrounded banners appearing does **not** exonerate the foreground path. Refine it to
**"do *foreground* banners appear from any app?"**: bring a sibling app frontmost (curator, or
Messages/Slack/Discord) and trigger one of *its* notifications while it is the active app. If that
banner is also missing → the OS foreground-presentation path is broken machine-wide (apply the fixes
below). If sibling apps *do* banner while frontmost but warden alone doesn't, that would be
warden-specific — but the trace above (delegate fires, returns `18`, usernoted accepts) leaves warden
nothing more to satisfy, so re-open the OS-registration angle (the two-paths `lsregister` note
below), not `notify.rs`.

## What the macOS-26 ("Tahoe") evidence points to

- macOS 26 has a **known, widespread notification-breakage regression** (notifications stop
  appearing despite being enabled; often Siri/widgets glitch too) from a corrupted notification
  background service. It is machine-wide, not app-specific.
- Corroborating on the affected machine: **both** of the author's sibling Tauri apps — warden
  (`au.lsjc.warden`) and curator (`au.lsjc.curator`) — were **absent from System Settings →
  Notifications**, even though warden was API-authorized. Two apps missing implicates the system's
  notification registration, not one app's code. (So "absent from the Settings list" is *not* a
  reliable warden-side signal on this OS version — don't chase it as a warden bug.)
- The machine's global gates were `dndMirrored = true` and `dndDisplaySleep = true` (Tahoe's
  default-to-block behaviour: suppress banners while mirroring/sharing the display, or while it
  sleeps). Neither was active during the test, but they are worth ruling out first.

### macOS-side fixes to try (warden needs no change), easiest first

1. **Reboot.** A plain restart often clears the corrupted notification service. (`killall
   ControlCenter NotificationCenter` *without* a re-login was observed **not** to fix it.)
2. System Settings → Notifications → bottom → turn **on** "Allow notifications when mirroring or
   sharing the display."
3. Clear the notification database (needs Full Disk Access for the terminal), then reboot:
   `rm -rf ~/Library/Group\ Containers/group.com.apple.UserNotifications/Library/UserNotifications/Remote/default/*`
4. Boot into **Safe Mode** (rebuilds caches), then restart normally.

A useful discriminator before any of this: do **foreground** banners from *other* apps (Messages,
curator) appear — i.e. with that app frontmost (see the foreground-only variant above; don't settle
for warden's own backgrounded banners working)? If nothing banners → system-wide breakage (the fixes
above). If others banner but warden doesn't → that *would* be warden-specific; re-open the
investigation (the LaunchServices dump did show warden registered at two paths — `/Applications` and
a build dir — clearable with a full `lsregister -kill -r -domain local -domain system -domain user`
rebuild, then run only the `/Applications` copy).

## Reproducing with the `notify_debug` trace

There is a config toggle to re-capture the path on demand (e.g. after a reboot):

```toml
notify_debug = true   # global; default false
```

It is read **once at launch** (a hot-reload change needs a restart), and only adds logging — the
production notification path is byte-for-byte unchanged when it's off (no completion handler is even
attached). When on, warden appends a trace to **`/tmp/warden-notify-dbg.log`**: the signal type, the
target tab and its visible/hidden state, whether `show_banner` posted, whether usernoted accepted,
and whether `willPresent` fired. A reproduction that shows all of those succeeding while no banner
appears on screen is the signature of the OS-side suppression above — and the proof to *not* edit
`notify.rs`.

`/tmp` is deliberate: a fresh log per boot matches the reboot-and-retry workflow. Turn the flag off
(or remove it) once the OS-side issue is resolved.
