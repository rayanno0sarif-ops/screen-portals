# Screen Portals

One mouse, keyboard and clipboard across the computers on your desk. Push the
cursor through the edge of one screen and it comes out on the next computer.
Everything stays on your local network and is encrypted end to end.

It runs on Windows 10 and 11, and on Linux desktops that use X11 (see
[Linux](#linux) for what that means and what is missing there). Windows and
Linux computers work together. The network protocol is platform neutral, so
other systems can be added without changing it.

## Quick start (the app)

Download `ScreenPortals.exe` from the [latest release](../../releases/latest),
copy it to both computers and start it on each. It sits
in the notification area; click its icon to open the settings window.
(On Linux the file is `ScreenPortals-linux-x86_64`; see [Linux](#linux).)

1. Press **Add a computer** on both.
2. Both show the same six-digit code. Press **Codes match** on each.
3. Drag the new computer to where it sits on your desk. The marked edge is
   where the mouse crosses. The other computer mirrors the arrangement.

Select a computer to choose where the mouse crosses:

| Option | The mouse crosses |
| --- | --- |
| Whole edge | anywhere along the edge, scaled if the screens differ in size |
| Where they touch | only along the stretch where the two screens meet |
| Top / left corner | only near that corner of the edge |
| Bottom / right corner | only near that corner of the edge |

### Files and pictures

| You do this | What happens |
| --- | --- |
| Copy text | It is on the other computer's clipboard at once. |
| Copy files, folders or a picture, then move the mouse across | They travel with the cursor and are ready to paste when they arrive. |
| Select a computer and press **Send files**, or drop files on its screen in the window | They are saved in `Downloads\Screen Portals` on that computer, which shows a notification. |

Copied files up to 1 GB in total are carried automatically; anything larger
stays behind and the window says so. Files sent with **Send files** have no
size limit. Nothing is ever overwritten: a second `report.pdf` is saved as
`report (2).pdf`. The window lists recent transfers, with progress, **Cancel**
and **Show in folder**.

### Dragging across

Hold what you are dragging and push the cursor through the marked edge:

| You drag | On the other computer |
| --- | --- |
| Files or folders | The drag carries on once the files have arrived; drop them where you like. Let go early and they are ready to paste instead. |
| A link or selected text | The drag carries on at once. |
| A browser window, by its title bar | The page it shows opens there. The window itself stays where it is. |

It works in both directions. A program's window cannot really move to another
computer, so only what a window *shows* can be reopened, and only for
browsers (Chrome, Edge, Firefox, Brave, Vivaldi, Opera). Dragging a single
tab out of a browser is not something browsers let other programs see, so
drag the window. Games that lock the mouse and programs running as
administrator do not take part.

### Updates

A copy downloaded from the releases page keeps itself up to date: every ten
minutes or so it asks this repository whether a newer build exists, downloads
it, checks it arrived intact, and restarts when nothing would be interrupted
(the cursor is at home and no files are moving). Start it with `--no-update`
to switch this off. Builds you make yourself never update.

Updates come over HTTPS from this repository on github.com and nowhere else,
so whoever can publish releases here decides what runs on the computers using
it. Keep the account that owns the repository secure.

### Linux

Download `ScreenPortals-linux-x86_64` from the
[latest release](../../releases/latest), then mark it as a program and
start it:

```
chmod +x ScreenPortals-linux-x86_64
./ScreenPortals-linux-x86_64
```

The settings window opens in your browser. There is no icon by the clock
yet: start the program again to bring the window back, and use **Quit** in
the window to stop it. `screen-portals-linux-x86_64` is the command-line
program; `./screen-portals-linux-x86_64 doctor` checks that the computer is
ready.

**It needs an X11 session.** Linux desktops come in two kinds, X11 (also
called Xorg) and Wayland. Wayland does not let an ordinary program watch
and move the mouse for the whole desktop, so Screen Portals refuses to
start there and says so. To find out which one you have:

```
echo $XDG_SESSION_TYPE
```

If it says `wayland`, sign out and look for a session called "Xorg" or
"X11" on the sign-in screen (often behind a gear icon). Some recent
systems no longer offer one; Screen Portals cannot run on those yet.

What works on Linux: the mouse, keyboard and wheel in both directions,
copied text, files and pictures, **Send files**, sound in both directions
(through PulseAudio or PipeWire, using the `parec`, `pacat` and `pactl`
tools most desktops already have), and automatic updates.

What does not, yet:

- Carrying a drag across. What you drag *onto* a Linux computer arrives
  ready to paste instead; dragging *from* Linux does not cross.
- Reopening a browser window's page on the other computer.
- An icon by the clock, and starting with the computer.
- Keyboards are passed on key by key, which assumes the usual Linux key
  numbering (true for ordinary Xorg sessions; remote-desktop X servers may
  differ).

### Sound

Under **Sound comes out of** in the window, choose one computer. Every other
paired computer then mutes its own speakers and sends what it plays to the
chosen one, so everything is heard from a single set of speakers or
headphones. **Each computer** switches this off. The choice only has to be
made on one computer; the others follow.

- Volume is set on the computer that plays. The muted computers' own volume
  sliders have no effect while their sound is sent away.
- Expect roughly a tenth of a second of delay: fine for music, video calls and
  notifications, noticeable in fast games.
- If the chosen computer is switched off or unreachable, the others play their
  own sound again until it is back.
- Sound travels encrypted like everything else, on the fast pointer path. On a
  network where that path is blocked, sound stays on its own computer.

Right-click the tray icon to pause sharing or quit. `ScreenPortals.exe --hidden`
starts without opening the window. The settings window is a local page shown
in Microsoft Edge's app mode (your default browser if Edge is missing); it is
served on 127.0.0.1 only and needs a per-run token.

To look at the settings window without sharing any real input, run
`cargo run --example ui_demo` and open the address it prints. Add `-- --slow`
to watch a large file on its way.

## Quick start (command line)

Download `screen-portals.exe` from the [latest release](../../releases/latest)
to both computers.

1. On the first computer, say where the second one sits:

   ```
   screen-portals pair --side right
   ```

2. On the second computer:

   ```
   screen-portals pair
   ```

3. Both show the same six-digit code. Check they match and answer `y` on each.

From then on just start `screen-portals run` on both. They find each other and
connect on their own. Push the mouse through the edge you chose to cross over;
`Ctrl+Alt+Home` always brings the cursor back.

If the computers cannot see each other automatically (some networks block
broadcasts), add `--connect <ip>:47811` on one of them.

Other commands: `peers` lists pairings, `forget <name>` removes one, `doctor`
checks the computer is ready (`doctor pointer` also moves the cursor briefly;
`doctor sound` tests capturing and playing sound, silencing the computer for
about a second).

## What works today

- Cursor crossing at any of the four edges, scaled between screen sizes
- Keyboard follows the cursor, including modifiers held while crossing
- Mouse buttons (five), vertical and horizontal wheel
- Clipboard text in both directions
- Copied files, folders and pictures follow the cursor in both directions
- Sending files from the settings window, with progress and cancel
- Sound from every computer played through one computer's speakers
- Dragging files, links and text through the edge, in both directions
- Reopening a browser window's page on the other computer
- Automatic updates from this repository's releases
- Automatic discovery and reconnection on the LAN
- Pairing with a six-digit code; unpaired devices are refused

## Not yet built

- Dropping whole folders on the window (copy and paste carries folders)
- Starting with the computer automatically
- Hiding the parked cursor while it is on the other computer
- Arranging individual monitors of a multi-monitor computer (each computer is
  one rectangle covering its whole desktop)
- Relative mouse movement for games
- macOS, and Linux desktops on Wayland
- Windows that run as administrator, and the lock/UAC screens, ignore
  simulated input unless Screen Portals itself is started as administrator

## How it is put together

| Part | File | What it does |
| --- | --- | --- |
| Identity and pairing | `config.rs`, `pairing.rs` | Each computer has a permanent X25519 key. First contact is confirmed with a code both people compare. |
| Control channel | `secure.rs` | Noise XX over TCP. Keys, clicks, clipboard and housekeeping travel here, in order. |
| Pointer channel | `datagram.rs` | Encrypted UDP. Each packet carries the newest position; late or replayed packets are dropped. Falls back to TCP if UDP is blocked. |
| Discovery | `discovery.rs` | A small LAN announcement saying "I am here". It is only a hint; the handshake decides who is trusted. |
| Edge logic | `layout.rs` | Pure geometry for leaving, travelling on and returning from the other desktop. |
| Files | `transfer.rs` | What may be sent, and the checks the receiving side makes before anything touches its disk. |
| Pictures | `picture.rs` | Copied pictures travel as PNG and are converted to and from the Windows clipboard bitmap. |
| Sound | `audio.rs`, `platform/win_audio.rs` | Captures what a computer plays, sends it as small encrypted packets, and smooths out network timing before playing it. |
| Dragging | `platform/win_drag.rs`, `platform/win_page.rs` | Takes over a drag at the edge, restarts it on arrival, and reads a browser window's page. |
| Updates | `update.rs`, `.github/workflows/release.yml` | Every push to `main` becomes a release; installed copies fetch, check and swap in the newest one. |
| Settings window | `ui.rs`, `ui/index.html` | A page served to this computer only; files chosen there are streamed through to the other computer. |
| Engine | `node.rs` | Ties it together and makes sure nothing stays stuck if a link drops. |
| Windows | `platform/windows.rs` | Low-level hooks to watch input, `SendInput` to play it back, clipboard listener. |
| Linux | `platform/linux.rs`, `platform/x11_clip.rs`, `platform/linux_audio.rs` | Watches input through XInput 2, grabs it while the cursor is away, plays it back with XTEST; the X11 clipboard; sound through the desktop's own tools. |
| Key codes | `keymap.rs` | Windows scan codes and Linux key codes to USB HID usages and back. |

### Design choices

- **Pairing code that cannot be steered.** Hashing the handshake into six
  digits is not enough on its own: someone in the middle can keep trying keys
  until the digits match. Here each side adds a random value and the dialling
  side commits to its value before seeing the other, so a match is wrong at
  most one time in a million.
- **Pointer on UDP, everything else on TCP.** A lost packet or a large
  clipboard transfer never makes the cursor wait.
- **Clicks carry their position.** Because movement and clicks use different
  channels, every click states where it happens.
- **Files travel when the cursor does.** Copying a large folder costs nothing
  until you actually move to the other computer. The newest copy always wins,
  whichever order slow transfers finish in.
- **A file never holds up typing.** File data shares the encrypted control
  channel, but the sender may only run 256 KB ahead of what the receiver has
  confirmed, so a key press is never queued behind more than that.
- **The receiver trusts nothing.** Paths are rebuilt from checked parts (no
  `..`, drive letters or device names), sizes are enforced, and everything is
  written to a fresh private folder that is removed again if the transfer
  fails or is cancelled. Copied files wait in the temporary folder; only
  files sent on purpose land in Downloads.
- **Sound cannot go in circles.** A computer that is sending its sound away
  never plays what it receives, and a computer is only muted while its sound
  is actually reaching the chosen one. Quitting or losing the network gives
  every computer its own sound back, and a computer that was switched off or
  crashed while muted is unmuted the next time Screen Portals starts.
- **Nothing sticks.** Keys and buttons held on the other computer are
  released when the cursor leaves or the connection drops, and the cursor
  comes home by itself.

## Repository layout

| Path | What is there |
| --- | --- |
| `src/` | The program. Everything outside `src/platform/` is portable and tested without real devices. |
| `src/platform/` | The Windows and Linux sides: watching and playing back input, clipboard, sound, drag and drop. |
| `ui/index.html` | The settings window (one self-contained page, built into the program). |
| `tests/loopback.rs` | End-to-end tests: whole engines talking over real sockets to pretend computers. |
| `tests/linux-desktop.sh` | The real Linux programs on two pretend displays: pairs them, pushes the mouse across, types, copies and comes back. |
| `examples/ui_demo.rs` | Preview the settings window against pretend computers. |
| `docs/` | The landing page, served by GitHub Pages. |
| `.github/workflows/` | Tests every change and publishes each push to `main` as a release. |

## Building

Needs the Rust toolchain (`rustup`), nothing else.

```
cargo test
cargo build --release
```

On Windows, build from a short folder path (or set `CARGO_TARGET_DIR` to one);
very long paths exceed the system's 260-character limit. On Linux nothing
else needs installing to build: the program speaks to the X server directly.

The tests run two or three complete engines against each other over real
sockets with a simulated screen, so they never touch your actual mouse or
clipboard. One extra check does use the real Windows clipboard and therefore
only runs when asked for (it puts copied text back afterwards, and does
nothing if files or a picture are on the clipboard):

```
cargo test --lib -- --ignored real_clipboard
```

## Ports

| Port | Use |
| --- | --- |
| UDP 47810 | discovery announcements |
| TCP 47811 | encrypted control channel, including files (`--port` changes it) |
| UDP, automatic | encrypted pointer channel |

Windows Firewall asks once to allow the program on private networks.
