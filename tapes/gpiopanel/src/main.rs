//! `gpiopanel`: the 40-pin header of a Raspberry Pi 4 on the touch screen, with a button for everything a pin can do. It works
//! on this machine's own pins (tape ABI 22, `gpio_*`) or, from any other fos machine, on the pins of a Pi at its shell prompt
//! over Ethernet (`host=NAME`; GPIO over the network, kernel/src/gpio_remote.rs, signed with the `synckey` file). The logic is
//! `crates/gpio-panel`; this file does the drawing, the touch and the hardware calls.
#![no_std]
#![no_main]

use audio_screen::{Screen, poll_keys};
use core::fmt::Write;
use gfx::Canvas;
use gpio_panel::header::{self, Pad};
use gpio_panel::wire::{FRAME_MAX, KEY_MAX, PINS, PULL_UNKNOWN, status_text};
use gpio_panel::{Client, Event, Hit, Layout, Panel, Rect, Request, State};
use tape_rt::{Key, KeyDecoder, Line, PeerInfo, TapeApi, TouchEvent, println};

tape_rt::tape_help!(
    br#"gpiopanel [host=NAME|MAC] [nic=N] [screen=N] [armed]
The 40-pin header of a Raspberry Pi 4 on the screen, a button for everything
a pin can do. Without host= it works on this machine's own pins (a Pi; the
`gpio-sim` setting gives a test machine a RAM bank). With host=NAME it works
on the pins of another fos machine, a Pi sitting at its shell prompt (not
running a tape), over Ethernet: NAME is its hostname plus the NIC number
(pi1), or a MAC address. Both machines need the same `synckey` file, and the
Pi needs `gpio-remote` in `settings` to take changes (reading always works).
nic=N picks our interface (default: the one the Pi was heard on).
Pads are coloured: blue input low, green input high, brown output low,
orange output high, violet alternate function, grey cannot be changed. Touch
a pad to select it; a second touch on an output flips it. The buttons set
function (an output starts low), level, pull; PULSE flips an output for
0.2 s. Changes need ARM first. Outputs keep their level when the tape ends.
Keys: arrows select, i o in/out, 0-5 alt0-5, h l high/low, space toggle,
p pulse, u d n pull up/down/none, a arm, Escape quits.
"#
);

const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;
const BACKGROUND: u32 = 0x0e1218;
const PANEL: u32 = 0x1a212b;
const EDGE: u32 = 0x4a5a70;
const INK: u32 = 0xd8e4f0;
const DIM: u32 = 0x7a8898;
const ACTIVE: u32 = 0x5ec8ff;
const BUTTON_OFF: u32 = 0x2a3340;
const WARN: u32 = 0xffb454;
const ARMED: u32 = 0xd84a3a;
/// Local pins are read this often.
const LOCAL_REFRESH_MS: u64 = 100;
/// "no answer" is shown after this many silent requests in a row.
const SILENT_LIMIT: u32 = 2;

fn now_ms() -> u64 {
    tape_rt::micros() / 1000
}

/// Where the pins are.
enum Source {
    Local { pull: [u8; PINS], next: u64 },
    Remote(Remote),
}

struct Remote {
    nic: usize,
    me: [u8; 6],
    client: Option<Client>,
    /// The name we are waiting to hear announced (`host=NAME`), until its MAC is known.
    looking_for: Option<&'static str>,
    key: [u8; KEY_MAX],
    key_length: usize,
    silent: u32,
    last_look: u64,
    refused: &'static str,
}

fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = text.split(':');
    for byte in &mut mac {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

fn find_peer(name: &str) -> Option<([u8; 6], usize)> {
    let table = tape_rt::api();
    if table.version < 5 {
        return None;
    }
    let mut info = PeerInfo::default();
    let count = (table.peer_info)(0, &mut info);
    (1..=count).find_map(|number| {
        (table.peer_info)(number, &mut info);
        info.name().eq_ignore_ascii_case(name).then_some((info.mac, usize::from(info.nic)))
    })
}

/// The first line of the `synckey` file, trimmed, as the kernel reads it.
fn read_key(out: &mut [u8; KEY_MAX]) -> Result<usize, &'static str> {
    let mut buffer = [0u8; 512];
    let name = b"synckey";
    let length = (tape_rt::api().fs_read)(name.as_ptr(), name.len(), buffer.as_mut_ptr(), buffer.len());
    if length < 0 {
        return Err("no `synckey` file here (one line of text, the same on the Pi)");
    }
    let text = core::str::from_utf8(&buffer[..length as usize]).map_err(|_| "synckey is not text")?;
    let line = text.lines().next().unwrap_or("").trim();
    if line.is_empty() || line.len() > KEY_MAX {
        return Err("the first line of `synckey` is empty or longer than 128 bytes");
    }
    out[..line.len()].copy_from_slice(line.as_bytes());
    Ok(line.len())
}

/// This machine's pins as the same `State` the network gives (`pull`: what this panel set, for kernels without pull read-back).
fn local_state(pull: &[u8; PINS]) -> State {
    let allowed = tape_rt::gpio_pins();
    let mut state = State { levels: tape_rt::gpio_read(), allowed, writable: allowed, function: [0; PINS], pull: *pull };
    for (pin, function) in state.function.iter_mut().enumerate() {
        *function = tape_rt::gpio_function(pin as u32).unwrap_or(0);
        // A kernel older than ABI 28 cannot say; then only the pulls this panel set are known.
        if let Some(read) = tape_rt::gpio_pull_get(pin as u32) {
            state.pull[pin] = read;
        }
    }
    state
}

fn local_error(code: i32) -> &'static str {
    match code {
        -1 => "that pin is not allowed",
        -2 => "bad argument",
        _ => "no GPIO here",
    }
}

fn apply_local(request: Request, pull: &mut [u8; PINS]) -> Result<(), &'static str> {
    match request {
        Request::State => Ok(()),
        Request::Set { mask, values } => match tape_rt::gpio_write(mask, values) {
            0 => Ok(()),
            _ => Err("that pin is not allowed or not an output"),
        },
        Request::Function { pin, function, level } => tape_rt::gpio_mode(u32::from(pin), u32::from(function), level).map_err(local_error),
        Request::Pull { pin, pull: kind } => {
            tape_rt::gpio_pull(u32::from(pin), u32::from(kind)).map_err(local_error)?;
            pull[usize::from(pin).min(PINS - 1)] = kind;
            Ok(())
        }
    }
}

fn fill(canvas: &mut Canvas, rect: Rect, rgb: u32) {
    canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, rgb);
}

fn frame_of(canvas: &mut Canvas, rect: Rect, rgb: u32) {
    canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, rgb);
}

fn center(canvas: &mut Canvas, rect: Rect, text: &str, scale: i32, rgb: u32) {
    let width = gfx::text_width(text) as i32 * scale;
    canvas.text_scaled(rect.x + (rect.w - width) / 2, rect.y + (rect.h - gfx::GLYPH_HEIGHT as i32 * scale) / 2, text, scale as u32, rgb);
}

fn pad_colour(state: Option<&State>, gpio: u8) -> u32 {
    let Some(state) = state else { return BUTTON_OFF };
    let high = state.level(gpio);
    if !state.is_allowed(gpio) {
        return if high { 0x8a9098 } else { 0x4a5058 };
    }
    match (state.function_of(gpio), high) {
        (0, false) => 0x2a5a98,
        (0, true) => 0x4ade80,
        (1, false) => 0x7a4a14,
        (1, true) => 0xffa630,
        _ => 0x8a5ad8,
    }
}

fn draw_connector(canvas: &mut Canvas, layout: &Layout, panel: &Panel) {
    fill(canvas, layout.plate, 0x12301f);
    frame_of(canvas, layout.plate, EDGE);
    let scale = layout.scale;
    let text_y = |pad: Rect| pad.y + (pad.h - gfx::GLYPH_HEIGHT as i32 * scale) / 2;
    for index in 0..40 {
        let pad = layout.pads[index];
        let (colour, name) = match header::HEADER[index] {
            Pad::V3 => (0xe0a030, "3V3"),
            Pad::V5 => (0xd04040, "5V"),
            Pad::Ground => (0x2a3038, "GND"),
            Pad::Gpio(gpio) => (pad_colour(panel.state.as_ref(), gpio), ""),
        };
        fill(canvas, pad, colour);
        let selected = matches!(header::HEADER[index], Pad::Gpio(gpio) if gpio == panel.selected);
        frame_of(canvas, pad, if selected { 0xffffff } else { 0x000000 });
        if selected {
            canvas.rect(pad.x - 1, pad.y - 1, pad.w as u32 + 2, pad.h as u32 + 2, 0xffffff);
        }
        let mut number = Line::<4>::new();
        let _ = write!(number, "{}", index + 1);
        center(canvas, pad, number.as_str(), 1, if colour == 0x4ade80 || colour == 0xffa630 || colour == 0xe0a030 { 0x000000 } else { 0xffffff });
        let mut label = Line::<24>::new();
        match header::HEADER[index] {
            Pad::Gpio(gpio) => {
                let _ = write!(label, "GPIO{gpio} {}", header::alt0_name(gpio));
            }
            _ => {
                let _ = write!(label, "{name}");
            }
        }
        let rgb = if selected { 0xffffff } else { INK };
        if index % 2 == 0 {
            canvas.text_right(layout.left_text, text_y(pad), label.as_str(), rgb);
        } else {
            canvas.text_scaled(layout.right_text, text_y(pad), label.as_str(), scale as u32, rgb);
        }
    }
}

fn lit(panel: &Panel, hit: Hit) -> bool {
    let Some(state) = panel.state.as_ref() else { return false };
    let pin = panel.selected;
    match hit {
        Hit::Function(function) => state.function_of(pin) == function,
        Hit::Level(level) => state.function_of(pin) == 1 && state.level(pin) == level,
        Hit::Pull(pull) => state.pull_of(pin) == pull,
        Hit::Arm => panel.armed,
        Hit::Pulse => panel.pulsing(),
        _ => false,
    }
}

fn draw_controls(canvas: &mut Canvas, layout: &Layout, panel: &Panel) {
    let usable = panel.armed && panel.state.as_ref().is_some_and(|state| state.is_writable(panel.selected));
    for button in layout.buttons {
        let rect = button.rect;
        let on = lit(panel, button.hit);
        let (fill_colour, ink) = match button.hit {
            Hit::Arm if on => (ARMED, 0xffffff),
            _ if on => (ACTIVE, 0x000000),
            Hit::Arm => (BUTTON_OFF, WARN),
            _ if usable => (BUTTON_OFF, INK),
            _ => (BUTTON_OFF, DIM),
        };
        fill(canvas, rect, fill_colour);
        frame_of(canvas, rect, EDGE);
        let label = if matches!(button.hit, Hit::Arm) && on { "ARMED" } else { button.label };
        center(canvas, rect, label, layout.scale, ink);
    }
}

fn draw_info(canvas: &mut Canvas, layout: &Layout, panel: &Panel) {
    let info = layout.info;
    fill(canvas, info, PANEL);
    frame_of(canvas, info, EDGE);
    let line_h = 16 * layout.scale + 2;
    let (x, mut y) = (info.x + layout.margin, info.y + 3);
    let pin = panel.selected;
    let mut line = Line::<64>::new();
    let _ = write!(line, "GPIO{pin}  (pad {})", header::number_of(pin).unwrap_or(0));
    canvas.text_scaled(x, y, line.as_str(), layout.scale as u32, ACTIVE);
    y += line_h;
    let Some(state) = panel.state.as_ref() else {
        canvas.text_scaled(x, y, "waiting for the pins...", layout.scale as u32, DIM);
        return;
    };
    let function = state.function_of(pin);
    line.clear();
    let _ = write!(line, "Function: {}", header::FUNCTION_NAMES[usize::from(function & 7)]);
    if function == 2 && !header::alt0_name(pin).is_empty() {
        let _ = write!(line, " ({})", header::alt0_name(pin));
    }
    canvas.text_scaled(x, y, line.as_str(), layout.scale as u32, INK);
    y += line_h;
    line.clear();
    let pull = state.pull_of(pin);
    let _ = write!(line, "Level: {}   Pull: {}", if state.level(pin) { "HIGH" } else { "low" }, if pull == PULL_UNKNOWN { "?" } else { header::PULL_NAMES.get(usize::from(pull)).copied().unwrap_or("?") });
    canvas.text_scaled(x, y, line.as_str(), layout.scale as u32, INK);
    y += line_h;
    let access = if !state.is_allowed(pin) {
        "Locked: this pin cannot change"
    } else if !state.is_writable(pin) {
        "Read only: no gpio-remote there"
    } else {
        "Can change"
    };
    canvas.text_scaled(x, y, access, layout.scale as u32, DIM);
    y += line_h;
    canvas.text_scaled(x, y, panel.note, layout.scale as u32, WARN);
}

fn draw_title(canvas: &mut Canvas, layout: &Layout, source: &Source) {
    fill(canvas, layout.title, PANEL);
    frame_of(canvas, layout.title, EDGE);
    let scale = layout.scale as u32;
    let y = (layout.title.h - gfx::GLYPH_HEIGHT as i32 * layout.scale) / 2;
    let x = canvas.text_scaled(layout.margin, y, "FOSFOR GPIO", scale, ACTIVE);
    let mut line = Line::<80>::new();
    let mut colour = INK;
    match source {
        Source::Local { .. } => {
            let _ = write!(line, "this machine");
        }
        Source::Remote(remote) => {
            if let Some(name) = remote.looking_for {
                let _ = write!(line, "looking for {name}...");
                colour = WARN;
            } else if !remote.refused.is_empty() {
                let _ = write!(line, "{}", remote.refused);
                colour = WARN;
            } else if remote.silent >= SILENT_LIMIT {
                let _ = write!(line, "no answer (is it at its prompt, same synckey?)");
                colour = WARN;
            } else {
                let _ = write!(line, "remote");
            }
        }
    }
    canvas.text_scaled(x + 4 * layout.margin, y, line.as_str(), scale, colour);
}

fn draw_footer(canvas: &mut Canvas, layout: &Layout) {
    let line_h = 16 * layout.scale + 2;
    let mut y = layout.footer_y();
    for text in ["Arrows: pin  a: arm  Esc: quit", "i o: in/out  0-5: alt  h l: high/low", "Space: toggle  p: pulse", "u d n: pull up/down/none"] {
        canvas.text_scaled(layout.info.x, y, text, layout.scale as u32, DIM);
        y += line_h;
    }
}

/// A finger lands once per touch; its movement and lift are not buttons.
struct Fingers {
    held: [Option<i32>; 8],
}

impl Fingers {
    fn pressed(&mut self, event: &TouchEvent) -> bool {
        if !event.down {
            for slot in &mut self.held {
                if *slot == Some(event.id) {
                    *slot = None;
                }
            }
            return false;
        }
        if self.held.contains(&Some(event.id)) {
            return false;
        }
        match self.held.iter_mut().find(|slot| slot.is_none()) {
            Some(slot) => {
                *slot = Some(event.id);
                true
            }
            None => false,
        }
    }
}

fn arg<'a>(arguments: &'a str, key: &str) -> Option<&'a str> {
    tape_rt::arg_value(arguments, key)
}

fn open_remote(arguments: &'static str) -> Result<Remote, &'static str> {
    let host = arg(arguments, "host").ok_or("")?;
    let mut key = [0u8; KEY_MAX];
    let key_length = read_key(&mut key)?;
    let mut remote = Remote { nic: tape_rt::active_nic(), me: [0; 6], client: None, looking_for: None, key, key_length, silent: 0, last_look: 0, refused: "" };
    if let Some(nic) = arg(arguments, "nic") {
        remote.nic = nic.parse().map_err(|_| "nic= needs a number")?;
    }
    if remote.nic >= (tape_rt::api().nic_count)() {
        return Err("no such network interface (nic=N)");
    }
    if let Some(mac) = parse_mac(host) {
        remote.client = Client::new(own_mac(remote.nic)?, mac, &remote.key[..key_length]);
        remote.me = own_mac(remote.nic)?;
    } else {
        remote.looking_for = Some(host);
    }
    Ok(remote)
}

fn own_mac(nic: usize) -> Result<[u8; 6], &'static str> {
    let mut mac = [0u8; 6];
    ((tape_rt::api().nic_mac)(nic, mac.as_mut_ptr()) == 1).then_some(mac).ok_or("that interface has no MAC address")
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = core::str::from_utf8(tape_rt::args()).unwrap_or("");
    if tape_rt::fp_level() == 0 {
        return println!("gpiopanel: needs a hard-float tape build (kernel without FP support?).");
    }
    let screen_number = tape_rt::screen_arg(arguments);
    let Some((width, height)) = tape_rt::gfx_info_on(screen_number) else {
        return println!("gpiopanel: {}.", if screen_number == 0 { "no pixel screen (BIOS text mode). Use a UEFI boot or a VBE mode" } else { "no such screen (screen=N)" });
    };
    let (width, height) = (width.min(MAX_WIDTH), height.min(MAX_HEIGHT));

    let mut source = if arg(arguments, "host").is_some() {
        match open_remote(arguments) {
            Ok(remote) => Source::Remote(remote),
            Err(reason) => return println!("gpiopanel: {reason}."),
        }
    } else {
        if tape_rt::gpio_pins() == 0 {
            return println!("gpiopanel: this machine has no GPIO pins; use host=NAME for a Pi's.");
        }
        Source::Local { pull: [PULL_UNKNOWN; PINS], next: 0 }
    };
    let layout = Layout::new(width as i32, height as i32);
    let Some(mut screen) = Screen::new(api, width, height).map(|screen| screen.on(screen_number)) else {
        return println!("gpiopanel: out of memory.");
    };
    if !screen.open() {
        return println!("gpiopanel: cannot take the screen.");
    }
    match &source {
        Source::Local { .. } => println!("gpiopanel: this machine's pins. Escape quits."),
        Source::Remote(_) => println!("gpiopanel: remote pins of {}. Escape quits.", arg(arguments, "host").unwrap_or("?")),
    }

    let mut panel = Panel::new();
    panel.armed = arguments.split_ascii_whitespace().any(|word| word == "armed");
    let mut decoder = KeyDecoder::new();
    let mut fingers = Fingers { held: [None; 8] };
    let mut receive = [0u8; 2048];
    let mut out = [0u8; FRAME_MAX];
    loop {
        let mut presses = [None::<Hit>; 8];
        let mut count = 0;
        let queue = |hit: Hit, presses: &mut [Option<Hit>; 8], count: &mut usize| {
            if *count < presses.len() {
                presses[*count] = Some(hit);
                *count += 1;
            }
        };
        let mut moved = false;
        let keep_going = poll_keys(api, &mut decoder, |key| match key {
            Key::Up => {
                panel.step(-1, 0);
                moved = true;
            }
            Key::Down => {
                panel.step(1, 0);
                moved = true;
            }
            Key::Left => {
                panel.step(0, -1);
                moved = true;
            }
            Key::Right => {
                panel.step(0, 1);
                moved = true;
            }
            Key::Char(c) => {
                let hit = match c {
                    b'i' => Hit::Function(0),
                    b'o' => Hit::Function(1),
                    b'0'..=b'5' => Hit::Function(2 + c - b'0'),
                    b'h' => Hit::Level(true),
                    b'l' => Hit::Level(false),
                    b' ' => Hit::Toggle,
                    b'p' => Hit::Pulse,
                    b'u' => Hit::Pull(1),
                    b'd' => Hit::Pull(2),
                    b'n' => Hit::Pull(0),
                    b'a' => Hit::Arm,
                    _ => return,
                };
                queue(hit, &mut presses, &mut count);
            }
            _ => {}
        });
        if !keep_going {
            break;
        }
        while let Some(event) = tape_rt::touch_event() {
            let Some(extent) = tape_rt::touch_extent(event.panel) else { continue };
            if event.screen >= 0 && event.screen != screen_number as i32 {
                continue;
            }
            if fingers.pressed(&event) {
                let (x, y) = (event.x * width as i32 / extent.0.max(1), event.y * height as i32 / extent.1.max(1));
                if let Some(hit) = layout.hit(x, y) {
                    queue(hit, &mut presses, &mut count);
                }
            }
        }

        let now = now_ms();
        let before = (panel.state.clone(), panel.selected, panel.armed, panel.note);
        let mut requests = [None::<Request>; 10];
        let mut pending = 0;
        for hit in presses.iter().flatten() {
            if let Some(request) = panel.press(*hit, now) {
                requests[pending] = Some(request);
                pending += 1;
            }
        }
        if let Some(request) = panel.tick(now) {
            requests[pending] = Some(request);
        }
        let mut link_changed = false;
        match &mut source {
            Source::Local { pull, next } => {
                let mut changed = false;
                for request in requests.iter().flatten() {
                    if let Err(note) = apply_local(*request, pull) {
                        panel.note = note;
                    }
                    changed = true;
                }
                if changed || now >= *next {
                    *next = now + LOCAL_REFRESH_MS;
                    panel.set_state(local_state(pull));
                }
            }
            Source::Remote(remote) => {
                if let Some(name) = remote.looking_for {
                    if now >= remote.last_look + 500 {
                        remote.last_look = now;
                        if let (Some((mac, heard_on)), Ok(me)) = (find_peer(name), own_mac(remote.nic)) {
                            let nic = if arg(arguments, "nic").is_some() { remote.nic } else { heard_on };
                            remote.nic = nic;
                            remote.me = own_mac(nic).unwrap_or(me);
                            remote.client = Client::new(remote.me, mac, &remote.key[..remote.key_length]);
                            remote.looking_for = None;
                            link_changed = true;
                        }
                    }
                }
                if let Some(client) = remote.client.as_mut() {
                    for request in requests.iter().flatten() {
                        if !client.submit(*request) {
                            panel.note = "Too many changes at once";
                        }
                    }
                    let mut events = [None::<Event>; 18];
                    let mut seen = 0;
                    let (length, event) = client.tick(now, &mut out);
                    if length > 0 {
                        (api.nic_send)(remote.nic, out.as_ptr(), length);
                    }
                    events[seen] = event;
                    seen += 1;
                    for _ in 0..16 {
                        let length = (api.nic_receive)(remote.nic, receive.as_mut_ptr(), receive.len());
                        if length == 0 {
                            break;
                        }
                        let (length, event) = client.receive(now, &receive[..length], &mut out);
                        if length > 0 {
                            (api.nic_send)(remote.nic, out.as_ptr(), length);
                        }
                        events[seen] = event;
                        seen += 1;
                    }
                    for event in events[..seen].iter().flatten() {
                        link_changed = true;
                        match *event {
                            Event::State => {
                                remote.silent = 0;
                                remote.refused = "";
                                if let Some(state) = client.state.clone() {
                                    panel.set_state(state);
                                }
                            }
                            Event::Done(0) => {}
                            Event::Done(status) => panel.note = status_text(status),
                            Event::Silent => remote.silent += 1,
                            Event::Refused(reason) => remote.refused = reason,
                        }
                    }
                }
            }
        }
        if moved || link_changed || before != (panel.state.clone(), panel.selected, panel.armed, panel.note) {
            screen.mark_dirty();
        }
        if screen.due() {
            let mut canvas = screen.frame();
            canvas.clear(BACKGROUND);
            draw_title(&mut canvas, &layout, &source);
            draw_connector(&mut canvas, &layout, &panel);
            draw_info(&mut canvas, &layout, &panel);
            draw_controls(&mut canvas, &layout, &panel);
            draw_footer(&mut canvas, &layout);
            screen.present();
        }
        tape_rt::idle();
    }
    screen.close();
    println!("gpiopanel: stopped. Outputs keep their level.");
}
