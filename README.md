# gtui-terms

A terminal UI serial console for Windows and Linux, built for ESP32 work but useful for anything on a UART.

What makes it ESP32-aware:

- Reset / bootloader control through DTR+RTS, with the sequence taken from `esp-idf-monitor` (including the Windows `usbser.sys` workaround where RTS changes are ignored unless DTR is re-written, and the separate USB-JTAG sequence for PID `0x1001`).
- Panic decoding: reads the register dump, filters the code addresses out of the PS/A/SAR/EXCVADDR noise, prints the ELF SHA256, and resolves each address with `addr2line` when you pass `--elf`.
- Core dump capture between the `CORE DUMP START/END` markers: preserves the UART text and decodes the base64 payload into a `.bin` file for `esp-coredump` raw input. Files go beside the log, or into the current directory without `--log`; invalid or incomplete payloads report an error and keep the text capture.
- External command hooks for flashing, `gdb`, or `idf.py coredump`, because those tools want the port to themselves.

Everything else a serial console needs: port picker, 8N1, auto-reconnect, line and raw input, timestamps, raw byte logging, HEX view, scrollback search, timed repeat sends, macros, and a config file.

## Build

```sh
cargo build --release
# -> target/release/gtui-terms
```

Windows: MSVC or the GNU toolchain both work (`rustup override set stable-x86_64-pc-windows-gnu` if you do not have Visual Studio). Linux: `libudev-dev` and `pkg-config` for USB VID/PID on the port picker; musl builds fall back to a sysfs scan.

## Use

```sh
gtui-terms                      # live port picker -> serial settings -> Apply
gtui-terms COM4 -b 921600       # Windows
gtui-terms /dev/ttyUSB0         # Linux
gtui-terms --demo --panic       # no hardware: simulated boot, progress bar and a panic dump
gtui-terms COM4 --elf build/app.elf --log run.log --timestamps
gtui-terms COM4 --data-bits 7 --parity even --stop-bits 2 --flow hardware
gtui-terms COM4 --no-reset --pipe   # script mode: stdin -> port, port -> stdout
gtui-terms COM4 --headless --session-log session.log  # agent: line commands and escaped records
```

With no configured port, the picker stays open even when no devices exist and refreshes every 500 ms. Selection follows the port name through reordering. Click a port or press Enter to review baud rate (including custom values), data bits, stop bits, parity, flow control, send EOL, and reset-on-connect before applying. `R` refreshes immediately; an explicit CLI/config port still connects directly.

The toolbar provides **Ports**, **Settings**, **Actions**, **ESP**, **Help**, **Quit**, **Save**, **Lock**, and **Packets**. Click it or use Tab, Left/Right, and Enter. `Ctrl+O` opens ports; `F3` opens settings; `F4` opens actions. Applying settings reconnects the current hardware port; Cancel discards the draft. Changes apply to the current session; use the config file for startup defaults. Click output to read without transmitting, then click input to send. Mouse wheel scrolls each output pane and menus. Settings, help, search, and action dialogs consume input even in RAW mode. `F1` or `/help` opens the scrollable command and shortcut reference.

Keys: `F1` help, `F2` line/raw toggle, `F3` settings, `F4` actions, `F5` reset, `F6` bootloader, `F7` search (prefix the query with `re:` for a regex), `F8` hex view, `Ctrl+O` ports, `PageUp/PageDown` scroll, `Ctrl+Home/Ctrl+End` jump to oldest / follow the tail, `Ctrl+N/Ctrl+P` next/previous search hit, `Ctrl+T` timestamps (toolbar `Time`, independent of packet view and log colours), `Ctrl+S` save session, `Ctrl+B` or Scroll Lock freeze/resume output, `Ctrl+L` or `/clear` clear the screen (toolbar `Clear`; capture files are kept), `Ctrl+Q` quit. Pending writes and active esptool operations must finish before quitting. In raw mode `Ctrl+]` returns to line mode.

Line commands: `/help`, `/clear`, `/ports`, `/settings`, `/esp`, `/elf <path>`, `/save [path]`, `/lock on|off`, `/timestamps on|off`, `/packets on|off` (each packet is split at its line breaks and keeps ESP log colours), `/packets gap <ms>` (idle gap that closes an RX packet, default 20, `0` = one packet per read), `/reset`, `/boot` (or `/bootloader`), `/seq D0 R1 W0.5` (custom DTR/RTS steps, `W` in seconds), `/eol crlf|lf|cr|none` (`Ctrl+R` cycles), `/hex 01 0d` (or `AA55`), `/txmode text|hex` (`Ctrl+X`: the input box sends HEX bytes without EOL; invalid HEX stays in the box), `/escapes on|off` (TEXT sends parse `\r \n \t \0 \\ \xHH`), `/rxcr newline|overwrite` (treat a lone RX CR as a line break instead of returning to column 0), `/dtr on|off`, `/rts on|off`, `/counters reset`, `/filter i,w,e` (show only the listed levels) or `/filter all`, `/repeat <ms> <count|0> <text>`, `/stop`, `/macro <name> <key> <text>`, `/hook <name>`, `/hooks`, `/quit`. `//text` sends a literal leading slash. Omitting `on|off` toggles Lock, Timestamps, or Packets.

The input box grows up to eight rows. `Enter` sends; `Shift+Enter` (where the terminal reports it), `Alt+Enter`, or `Ctrl+J` inserts a newline, and bracketed paste keeps pasted lines in the box instead of sending them. Multi-line data is sent with the selected EOL between and after lines (with EOL `none`, typed newlines are sent as `\n`); a multi-line block starting with `/` runs one command per line. `Up/Down` move between input rows and recall history at the first/last row. Editing keys: `Ctrl+A/E` line start/end, `Ctrl+W` or `Ctrl+Backspace` delete a word, `Ctrl+U/K` delete to line start/end, `Ctrl+Left/Right` move by word.

The status bar shows the frame format (`921600 baud 8N1`), RX/TX byte totals with a 500 ms RX rate, dropped lines, and the active filter. Command history is kept in `%APPDATA%\gtui-terms\history.txt` (or `$XDG_CONFIG_HOME/gtui-terms/history.txt`). If no `--elf` is given, `build/*.elf` is picked up automatically.

ANSI rendering supports 16/256/RGB foreground and background colours, bold, underline and their resets. Invalid UTF-8 bytes appear as exact `\xNN` escapes; the status bar reports their cumulative count as `invalid UTF-8 N`.

Plain ESP-IDF log prefixes also receive level colors: `I` green, `W` yellow, `E` red, `D` cyan, and `V` gray; Arduino log prefixes are recognized too. Explicit ANSI foreground colors take precedence.

RX appears on the left and successfully written TX on the right. Narrow windows omit an empty TX pane. **Packets** shows the direction, byte count, and escaped content of each serial read/write chunk; these chunks are not protocol frame boundaries. `F8` switches byte contents to HEX. **Lock** preserves the visible position while receiving new data, including when earlier history is evicted; unlocking returns to live output. Timestamps are local arrival times in the UI.
`F7` and search navigation switch from packet view to decoded text so search hits map to visible lines.

**Save** / `Ctrl+S` opens a destination dialog. `/save path.log` exports directly; `/save` chooses a timestamped filename beside the raw log or in the current directory. Capture starts with the application, spools to a temporary file, and includes RX, successful TX, and ESP tool output even after the display is cleared or its bounded history is evicted. Exports use UTC timestamps, direction, length, exact HEX, and escaped text, never overwrite an existing file, and report incomplete capture instead of silently exporting missing data. A later save includes later records. `--session-log path.log` exports on normal exit; `--log` remains raw RX capture.

## Agent terminal use

`--headless` works with a real port or `--demo` and uses standard input/output without an alternate screen, mouse capture, ANSI UI, or keyboard escape sequences. Each input line is a payload or local slash command, with the same configured serial parameters and EOL as the TUI. It can run under a terminal manager such as herdr without requiring a manager-specific API. `--pipe` remains the raw byte passthrough mode.

Output is one escaped record per line: `UTC direction="RX|TX|TOOL|COMMAND|STATE|CONFIRM|HELP" length=N hex="..." text="..."`. HEX preserves exact bytes; text escapes newlines, quotes, backslashes, and non-ASCII bytes. A `COMMAND` record acknowledges receipt, while `TX` confirms successful serial write/flush. `STATE` reports connection/tool results. EOF stops repeats, waits for pending serial commands and tools, and exits; `--session-log` then exports the session. A normal process exit means the loop/export completed; inspect `STATE` and `TOOL` for individual command failures. `/ports` lists current devices, `/settings` reports current serial parameters, and `/help` prints supported syntax.

For automation, issue `/esp chip-id`, `/esp flash-id`, or `/esp read-mac` for read-only diagnostics. `/esp flash build`, `/esp erase-flash` and `/esp write-flash 0x1000 "bootloader.bin" 0x10000 "app.bin"` emit a `CONFIRM` preview; only a subsequent `/confirm` launches the operation. `/cancel` discards it. Closing stdin with a pending preview does not run it.

## ESP-IDF workflow

Open **ESP** on the toolbar or run `/esp`. The panel shows the device, active ELF, automatic panic/coredump status, capture directory, reset wiring, and configured hooks. Select reset, bootloader, ELF loading, chip/Flash information, MAC reading, Flash erase, multi-image flashing, or a configured hook with the keyboard or mouse.

1. Load the matching application ELF with debug symbols through **Load ELF** or `/elf build/app.elf`. Panic detection runs on incoming UART data; matching symbols enable address/source decoding, and an ELF SHA mismatch is reported.
2. Enable UART base64 coredumps in the firmware to capture real dumps. The console saves both `.txt` and decoded `.bin` beside the raw log, or in the current directory without logging. Feed the binary into the appropriate `esp-coredump` tooling with the matching ELF.
3. Configure hooks for `idf.py`, `esptool`, or GDB, and install those tools on PATH. Selecting a hook runs its actual shell command after releasing the port; the console reconnects afterward. Configured regex triggers still run automatically. The ESP panel displays the active hook and last detection result.
4. Built-in ESP actions use the bundled [espflash](https://github.com/esp-rs/espflash) library: no Python, esptool, or ESP-IDF installation is needed. The port is released during execution and reopened afterward; progress (per image, 10% steps), verification, and errors appear in output and session capture. Read-only actions use the ROM directly without uploading a stub; erase and write upload the stub, write compressed, verify, and skip images whose Flash contents already match.
   - **One-click flash**: `/esp flash <path>` or the ESP panel's *Flash firmware* prompt (pre-filled with `build` when `./build/flasher_args.json` exists). A directory containing `flasher_args.json` (an ESP-IDF `build/` folder) writes exactly the listed images at their offsets. An `.elf` file (or a directory with a single `.elf`) is converted to an app image; `bootloader/bootloader.bin` and `partition_table/partition-table.bin` beside it are used when present, otherwise espflash's bundled bootloader and a generated default partition table are written, so no partition table has to be prepared by hand. The ELF path writes the detected Flash size into the image header.
   - `/esp write-flash <offset> <file> ...` writes raw images; `/esp erase-flash` erases the chip.
   - Erase and write run at `flash_baud` (default 460800, `/esp baud <n>` or config `flash_baud`), read-only queries at 115200. Destructive actions display the target, image offsets, and resolved paths before explicit confirmation; quoted paths with spaces and multiple images are supported. Invalid, empty, overlapping, or out-of-range image inputs are rejected before execution.
   - The library comes from a [maintained espflash fork](https://github.com/NewNoob2002/espflash) (pinned by git revision). It skips the security-info probe that the classic ESP32 ROM never answers, cutting connect time from about 6 s to about 2 s.

The clickable toolbar and separate configuration/action dialogs follow the interaction pattern of [termapy](https://github.com/hucker/termapy).

## Config

Looked up as `./.gtui-terms.toml`, then `%APPDATA%\gtui-terms\config.toml` (or `$XDG_CONFIG_HOME`), overridden by `--config`:

```toml
port = "COM4"
baud = 921600
timestamps = true
packet_gap_ms = 20   # idle gap that closes an RX packet in packet view; 0 = one packet per read
flash_baud = 460800 # built-in espflash erase/write baud; read-only queries use 115200
elf = "build/app.elf"

[[macros]]
name = "version"
key = "alt+v"
text = "at+ver?"

[[hooks]]
name = "flash"
command = "idf.py -p COM4 flash"

[[hooks]]
name = "backtrace"
command = "idf.py -p COM4 coredump"
trigger = "Guru Meditation Error"
```

Create or replace a macro with `/macro version alt+v at+ver?`. It saves to the `--config` file, or `./.gtui-terms.toml`, and the binding works immediately. Keys are `f9`–`f12` or `alt+<char>`; a later definition with the same name overrides an earlier one. Saving preserves other TOML values, including hooks, but normalizes formatting and removes comments. Invalid configurations and failed saves leave the active macro unchanged.

A hook closes the port first, runs the command through `cmd /C` (Windows) or `sh -c`, prints its output into the scrollback, and reconnects. Hooks with a `trigger` regex fire automatically when a received line matches, at most once every five seconds.

## Known limits

Windows GNU with a CH340 (`1a86:7523`) and ESP32-D0WD-V3 has been checked at 115200 8N1: boot/log capture, hardware reset, download-mode entry, binary ROM SYNC round trip (92 bytes sent, 112 bytes received), and recovery to the existing ESP-IDF application. A read-only esptool ROM diagnostic also identified the chip. Sustained throughput, other boards, Linux, and real Xtensa/RISC-V symbolization remain unverified. ANSI support covers SGR, `ESC[K/D/C/G` and OSC skipping rather than full VT100 emulation. Coredump decoding is byte-verified with synthetic UART captures; real-board dumps have not been checked with `esp-coredump`. Linux with a CH340 and ESP32-D0WD-V3 (v3.1, 8MB): built-in chip info, `/esp flash build/` (all images unchanged and skipped by checksum), and `/esp flash build/app.elf` (bootloader and app rewritten at 460800 baud, verified, application booted) have been checked. Erase-flash and generated default partition tables remain unverified on hardware.
