//! The nettest> prompt's help: one short overview, one page per topic, and `help all`. Pages are
//! written for an 80x25 text grid (the BIOS console, a serial terminal) and every line is at most 79
//! columns; a text longer than the console's rows is shown a screenful at a time.

use tape_rt::api;

type Putc = extern "C" fn(u32);

const EVENT_ABORT: u32 = 1;
const EVENT_BYTE_BASE: u32 = 0x100;

const OVERVIEW: &[u8] = b"\
nettest measures a network, or breaks it on purpose. Pick a mode, then type
commands; nothing runs until you do. 'nics' lists the NICs.

  link A B             impair the wire between two NICs: loss, delay, rate...
  gen / sink           one machine: send test traffic out of one NIC, count it
                       on another (cabled together)
  serve N | remote N   two machines: the far end, and the master that runs the
                       tests and prints one report (then: peer, test)

Anywhere: show  stats  summary  save NAME  view on|off  reset  nics  quit
More:     help start | link | gen | two | check | sweep | all
Escape stops the tape.
";

const START: &[u8] = b"\
Getting started. Nothing runs until you pick a mode ('nics' lists the NICs).

 The wire in the middle: cable machine A to NIC 2 and machine B to NIC 3.
   link 2 3            then:  loss 2   delay 40 5   rate 10000
   'show' lists the faults, 'stats' counts, 'reset' makes the wire clean.

 One machine, two NICs cabled together:
   gen nic 2   sink nic 3   gen rate 5000   gen start        then: summary
   (or: gen sweep 1000 9000 1000 3 steps the rate and prints a table)

 Two machines, any wire or switch between them:
   far end:  serve 1                 master:  remote 1  (finds it; P picks)
   master:   test check            (the health check: GOOD or PROBLEM)
             test both rate=10000 time=5       or   test sweep both to=20000

 Roles can be combined (a link on two NICs, gen/sink on two others); serve and
 remote use one NIC and nothing else. 'link off', 'gen nic off', 'sink nic off'
 and 'serve off' / 'remote off' give a NIC back.
";

const LINK: &[u8] = b"\
The impaired wire (link A B). Prefix ab or ba for one direction only.
  loss [ab|ba] PCT [burst N]   drop PCT% (0.1 steps); N lost in a row
  delay [ab|ba] MS [JITTER]    hold frames MS ms +/- JITTER ms
  rate [ab|ba] KBIT            link speed; 0 = unlimited
  queue [ab|ba] N              frames allowed in flight
  dup|reorder|corrupt [ab|ba] PCT   deliver twice / swap with next / flip a bit
  preset [ab|ba] NAME          a named profile; \"presets\" lists them
  seed N                       repeatable randomness
  show | stats | reset [stats] settings; counters; clean wire / clear counters
  view on|off                  live statistics panel above the prompt
  link A B | link off          which two NICs carry the wire
";

const GEN: &[u8] = b"\
One machine: the generator (gen nic N) and the sink (sink nic N).
  gen start | stop | show      the traffic generator
  gen nic N|off                which NIC sends; sink nic N|off which one counts
  gen rate KBIT | size BYTES   constant bit rate of whole frames (60-1514)
  gen sweep START END STEP S   step the rate, S seconds each; table of results
  gen bidir on|off             also send a stream back, sink NIC -> gen NIC
  gen to MAC|sink | count N | time S    destination; stop after N frames / S s
  gen ip|dst|gw A.B.C.D | port N        send routable UDP/IP (ARPs the gateway)
  sink [show|hist|reset]       loss, latency, jitter; the latency histogram
  sink ip A.B.C.D | port N     also accept UDP/IP; answer ARP for the address
  summary | save NAME          end-of-test summary; write a report file
";

const TWO: &[u8] = b"\
Two machines: run nettest on both. Unicast only; other hosts are not disturbed.
  serve N | serve off      far end on NIC N: waits for a master and answers it
  remote N | remote off    master on NIC N: probes the announced machines
  peer                     probe again; 'peer N|NAME|MAC' picks one, P cycles
  test check               the health check: ~40 s, one answer GOOD or PROBLEM
  test [up|down|both] [rate=KBIT] [size=BYTES|mix] [time=SECONDS]   one test
  test sweep | soak | burst   step the rate | a long run | back-to-back frames
                           ('test sweep help', 'help soak', 'help burst')
  min-rate=KBIT max-loss=PERCENT   make a test print RESULT: PASS or FAIL
  source-macs=N            rotate the source address through N addresses
  peer-nic=N our-nic=N     test the peer's NIC N / our NIC N (kept until
                           changed; 0 = the control NIC)
  test stop                abort a running test (Escape does too; a second
                           Escape leaves nettest)
Each test starts from the defaults (both ways, 900000 kbit/s, 1400 B, 5 s).
The report begins with a Summary line; below it, per direction: frames, loss,
rate, latency, queueing delay, delivery per second. 'help check' says more.
";

const SWEEP: &[u8] = b"\
test sweep: step the offered rate and find where the link stops keeping up.
  test sweep [both|up|down] [from=KBIT] [to=KBIT] [step=KBIT] [time=SECONDS]
             [pause=SECONDS] [size=BYTES|mix] [source-macs=N]
             [min-rate=KBIT] [max-loss=PERCENT]
  from      first rate, default 100000 (a tenth of a gigabit link)
  to        last rate, default 1000000 (the whole link)
  step      increase per step, default 100000
  time      seconds per step, default 3
  pause     seconds of rest between steps so the queue drains, default 1
One row per step: kbit/s delivered, loss and queueing delay for each direction,
then the highest rate with at most 1% loss. The first step that loses frames
is the link's knee. On a 100 Mbit link: from=10000 to=100000 step=10000.
 example:  test sweep up from=50000 to=500000 step=50000
";

const SOAK: &[u8] = b"\
test soak: a long run in 10 s segments, to qualify a cable or a switch.
  test soak [both|up|down] [rate=KBIT] [size=BYTES|mix] [time=SECONDS]
            [source-macs=N] [min-rate=KBIT] [max-loss=PERCENT]
  rate      default 900000      size   default 1400
  time      total seconds, default 600 (up to 86400)
A line per segment (kbit/s, loss, error counters), then a summary with the
worst second and the seconds under 95% of the rate. Add min-rate and max-loss
for RESULT: PASS or FAIL. 'test stop' or Escape ends it. It takes about a
quarter longer than time. Name the direction (both): up/down are not remembered.
";

const BURST: &[u8] = b"\
test burst: how many back-to-back frames survive, to size a switch's buffer.
  test burst [both|up|down] [from=FRAMES] [to=FRAMES] [size=BYTES]
  from      first burst, default 32 frames; each step doubles it
  to        last burst, default 8192 frames
  size      bytes per frame, default 1400
Frames go out unpaced; each step prints delivered and lost per direction. A
switch only drops a burst when its output is slower than its input or shared
(1G into 100M, two senders into one port); the largest burst delivered whole
is about its buffer. On a clean 1G to 1G path nothing is lost, which means the
switch never queued.
";

const CHECK: &[u8] = b"\
test check: one command to find out whether a link or switch is healthy.
  test check [rate=KBIT]      rate defaults to 900000 (or what the link carries)
  1  full-size frames (1400 B) at 900 Mbit/s for 10 s, both ways
  2  mixed sizes (64/576/1500 B) from 64 source addresses, 10 s
  3  bursts of back-to-back frames, 32 doubling to 512
It reads the link speed, duplex and error counters of both NICs. The answer:
  GOOD          no loss, full rate, no errors
  PROBLEM       with the reasons, and what to try
  INCONCLUSIVE  the tape could not send that fast; lower the rate
For a long run use 'test soak', for a rate ladder 'test sweep'.
";

/// The page for `topic` ("" = the overview); None for an unknown topic.
fn page(topic: &[u8]) -> Option<&'static [&'static [u8]]> {
    Some(match topic {
        b"" => &[OVERVIEW],
        b"start" | b"modes" => &[START],
        b"link" => &[LINK],
        b"gen" | b"sink" => &[GEN],
        b"two" | b"remote" | b"serve" | b"test" | b"peer" => &[TWO],
        b"check" => &[CHECK],
        b"sweep" => &[SWEEP],
        b"soak" => &[SOAK],
        b"burst" => &[BURST],
        b"all" => &[OVERVIEW, START, LINK, GEN, TWO, CHECK, SWEEP, SOAK, BURST],
        _ => return None,
    })
}

/// Prints help for `topic`; false (and a hint) if there is no such topic.
pub fn print_topic(putc: Putc, topic: &[u8]) -> bool {
    let Some(pages) = page(topic) else {
        for &byte in b"no such topic; help start | link | gen | two | check | sweep | all\n" {
            putc(u32::from(byte));
        }
        return false;
    };
    let (_, rows) = tape_rt::console_size();
    let screen = rows.saturating_sub(1).max(10);
    let mut shown = 0;
    for (index, text) in pages.iter().enumerate() {
        if index != 0 {
            putc(u32::from(b'\n'));
            shown += 1;
        }
        for line in text.split_inclusive(|&byte| byte == b'\n') {
            if shown >= screen && !more(putc) {
                return true;
            }
            if shown >= screen {
                shown = 0;
            }
            for &byte in line {
                putc(u32::from(byte));
            }
            shown += 1;
        }
    }
    true
}

/// Waits for a key after a full screen; false if the reader wants to stop.
fn more(putc: Putc) -> bool {
    for &byte in b"-- more: any key for the next page, q stops --" {
        putc(u32::from(byte));
    }
    let go = loop {
        let event = (api().read_key)();
        if event == EVENT_ABORT {
            break false;
        }
        if event >= EVENT_BYTE_BASE {
            break !matches!((event - EVENT_BYTE_BASE) as u8, b'q' | b'Q');
        }
        (api().wait_for_event)();
    };
    putc(u32::from(b'\r'));
    for _ in 0..48 {
        putc(u32::from(b' '));
    }
    putc(u32::from(b'\r'));
    go
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_line_fits_79_columns() {
        for text in [OVERVIEW, START, LINK, GEN, TWO, CHECK, SWEEP, SOAK, BURST] {
            for line in text.split(|&byte| byte == b'\n') {
                assert!(line.len() <= 79, "too long ({}): {}", line.len(), core::str::from_utf8(line).unwrap());
            }
        }
    }

    #[test]
    fn topic_pages_fit_a_25_row_console() {
        for text in [OVERVIEW, START, LINK, GEN, TWO, CHECK, SWEEP, SOAK, BURST] {
            assert!(text.iter().filter(|&&byte| byte == b'\n').count() <= 22);
        }
    }
}
