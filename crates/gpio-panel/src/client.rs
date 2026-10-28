//! A GPIO-over-the-network client that never blocks the screen: it holds one request at a time, asks the machine for a
//! challenge, signs the request with it and checks the signed answer. The tape calls `tick` every pass (it may return a
//! frame to send) and `receive` for every frame that arrives; both may return an `Event`. Two frames per request, so a
//! state poll every 150 ms is about 13 frames a second.

use crate::wire::{self, KEY_MAX, NONCE_LEN, OP_SET, REPLY_LABEL, REQUEST_LABEL, Request, STATUS_NO_GPIO, STATUS_OK, STATUS_REFUSED, State, TAG_REQUEST};

/// Silence after which a request is given up.
const TIMEOUT_MS: u64 = 1200;
/// Between state polls.
const POLL_MS: u64 = 150;
const QUEUE: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    State,
    /// A change was answered with this status (0 done).
    Done(u8),
    /// Nobody answered (or not in time).
    Silent,
    /// An answer arrived that is not ours (wrong key) or the machine has no pins.
    Refused(&'static str),
}

#[derive(Clone, Copy)]
enum Step {
    Idle,
    Challenge { since: u64, request: Request },
    Reply { since: u64, request: Request, nonce: [u8; NONCE_LEN] },
}

pub struct Client {
    me: [u8; 6],
    peer: [u8; 6],
    key: [u8; KEY_MAX],
    key_length: usize,
    step: Step,
    queue: [Option<Request>; QUEUE],
    next_poll: u64,
    /// The newest state the machine reported.
    pub state: Option<State>,
}

impl Client {
    /// `key` is the first line of `synckey`; `None` when it is empty or too long.
    pub fn new(me: [u8; 6], peer: [u8; 6], key: &[u8]) -> Option<Client> {
        if key.is_empty() || key.len() > KEY_MAX {
            return None;
        }
        let mut copy = [0u8; KEY_MAX];
        copy[..key.len()].copy_from_slice(key);
        Some(Client { me, peer, key: copy, key_length: key.len(), step: Step::Idle, queue: [None; QUEUE], next_poll: 0, state: None })
    }

    /// Queues a change; false when the queue is full. The state is polled again right after it.
    pub fn submit(&mut self, request: Request) -> bool {
        match self.queue.iter_mut().find(|slot| slot.is_none()) {
            Some(slot) => {
                *slot = Some(request);
                true
            }
            None => false,
        }
    }

    pub fn busy(&self) -> bool {
        !matches!(self.step, Step::Idle)
    }

    fn key(&self) -> &[u8] {
        &self.key[..self.key_length]
    }

    fn next_request(&mut self, now: u64) -> Option<Request> {
        if let Some(first) = self.queue.iter().position(Option::is_some) {
            let request = self.queue[first].take();
            self.queue.copy_within(first + 1.., first);
            self.queue[QUEUE - 1] = None;
            self.next_poll = now;
            return request;
        }
        (now >= self.next_poll).then(|| {
            self.next_poll = now + POLL_MS;
            Request::State
        })
    }

    /// Called every pass. Returns the length of a frame written to `out` (0: nothing to send) and an event if a request
    /// timed out.
    pub fn tick(&mut self, now: u64, out: &mut [u8; wire::FRAME_MAX]) -> (usize, Option<Event>) {
        match self.step {
            Step::Challenge { since, .. } | Step::Reply { since, .. } if now.wrapping_sub(since) > TIMEOUT_MS => {
                self.step = Step::Idle;
                self.next_poll = now + POLL_MS;
                (0, Some(Event::Silent))
            }
            Step::Idle => match self.next_request(now) {
                Some(request) => {
                    self.step = Step::Challenge { since: now, request };
                    (wire::build(out, self.peer, self.me, TAG_REQUEST, &[0]), None)
                }
                None => (0, None),
            },
            _ => (0, None),
        }
    }

    /// Every received frame goes here; one that is not an answer to the request in flight is ignored. May write the signed
    /// request to `out`.
    pub fn receive(&mut self, now: u64, frame: &[u8], out: &mut [u8; wire::FRAME_MAX]) -> (usize, Option<Event>) {
        let Some(payload) = wire::reply_from(frame, self.peer) else { return (0, None) };
        match self.step {
            Step::Idle => (0, None),
            Step::Challenge { request, .. } => {
                if payload[0] == STATUS_NO_GPIO {
                    self.step = Step::Idle;
                    return (0, Some(Event::Refused("that machine has no GPIO pins")));
                }
                if payload[0] != STATUS_OK || payload.len() < 1 + NONCE_LEN {
                    return (0, None);
                }
                let nonce: [u8; NONCE_LEN] = payload[1..1 + NONCE_LEN].try_into().unwrap();
                let mut args = [0u8; 16];
                let (op, length) = request.encode(&mut args);
                let mut body = [0u8; 1 + NONCE_LEN + 16 + wire::MAC_LEN];
                body[0] = op;
                body[1..1 + NONCE_LEN].copy_from_slice(&nonce);
                body[1 + NONCE_LEN..1 + NONCE_LEN + length].copy_from_slice(&args[..length]);
                let end = 1 + NONCE_LEN + length;
                body[end..end + wire::MAC_LEN].copy_from_slice(&wire::mac(REQUEST_LABEL, op, &nonce, &args[..length], self.key()));
                self.step = Step::Reply { since: now, request, nonce };
                (wire::build(out, self.peer, self.me, TAG_REQUEST, &body[..end + wire::MAC_LEN]), None)
            }
            Step::Reply { request, nonce, .. } => {
                let mut args = [0u8; 16];
                let (op, _) = request.encode(&mut args);
                let status = payload[0];
                let size = if status == STATUS_OK || (op == OP_SET && status == STATUS_REFUSED) { wire::data_len(op) } else { 0 };
                if payload.len() < 1 + NONCE_LEN + size + wire::MAC_LEN || payload[1..1 + NONCE_LEN] != nonce {
                    return (0, None);
                }
                let data = &payload[1 + NONCE_LEN..1 + NONCE_LEN + size];
                let signature = &payload[1 + NONCE_LEN + size..1 + NONCE_LEN + size + wire::MAC_LEN];
                self.step = Step::Idle;
                if !tape_sync::equal(&wire::mac(REPLY_LABEL, status, &nonce, data, self.key()), signature) {
                    return (0, Some(Event::Refused("the answer is not signed with our key")));
                }
                if op == wire::OP_STATE {
                    return match State::decode(data) {
                        Some(state) => {
                            self.state = Some(state);
                            (0, Some(Event::State))
                        }
                        None => (0, Some(Event::Refused("the state answer is malformed"))),
                    };
                }
                self.next_poll = now;
                (0, Some(Event::Done(status)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{FRAME_MAX, MAC_LEN, OP_STATE, PINS, STATUS_NO_WRITE, TAG_REPLY};

    const ME: [u8; 6] = [2, 0, 0, 0, 0, 1];
    const PI: [u8; 6] = [2, 0, 0, 0, 0, 9];

    /// A stand-in for the kernel's server, written from the protocol description.
    struct Server {
        key: &'static [u8],
        nonce: [u8; NONCE_LEN],
        writes: bool,
        levels: u64,
    }

    impl Server {
        fn answer(&mut self, request: &[u8]) -> Vec<u8> {
            assert_eq!(request[14], TAG_REQUEST);
            let payload = &request[15..];
            let mut frame = vec![0u8; 15];
            frame[..6].copy_from_slice(&ME);
            frame[6..12].copy_from_slice(&PI);
            frame[12..14].copy_from_slice(&wire::ETHERTYPE);
            frame[14] = TAG_REPLY;
            if payload[0] == 0 {
                frame.push(STATUS_OK);
                frame.extend_from_slice(&self.nonce);
                return frame;
            }
            let op = payload[0];
            let nonce = &payload[1..1 + NONCE_LEN];
            assert_eq!(nonce, self.nonce);
            let args_length = match op {
                OP_STATE => 0,
                OP_SET => 16,
                3 => 3,
                _ => 2,
            };
            let args = &payload[1 + NONCE_LEN..1 + NONCE_LEN + args_length];
            let signature = &payload[1 + NONCE_LEN + args_length..1 + NONCE_LEN + args_length + MAC_LEN];
            assert_eq!(signature, wire::mac(REQUEST_LABEL, op, nonce, args, self.key));
            let mut data = vec![];
            let status = match op {
                OP_STATE => {
                    data.extend_from_slice(&self.levels.to_le_bytes());
                    data.extend_from_slice(&0x0fff_fffcu64.to_le_bytes());
                    data.extend_from_slice(&(if self.writes { 0x0fff_fffcu64 } else { 0 }).to_le_bytes());
                    data.extend((0..PINS).map(|pin| u8::from(pin == 7)));
                    data.extend((0..PINS).map(|pin| if pin == 9 { 1 } else { 0 }));
                    STATUS_OK
                }
                OP_SET if self.writes => {
                    let (mask, values) = (u64::from_le_bytes(args[..8].try_into().unwrap()), u64::from_le_bytes(args[8..].try_into().unwrap()));
                    self.levels = self.levels & !mask | values & mask;
                    data.extend_from_slice(&0u64.to_le_bytes());
                    STATUS_OK
                }
                _ => STATUS_NO_WRITE,
            };
            frame.push(status);
            frame.extend_from_slice(nonce);
            frame.extend_from_slice(&data);
            frame.extend_from_slice(&wire::mac(REPLY_LABEL, status, nonce, &data, self.key));
            frame
        }
    }

    fn server(key: &'static [u8], writes: bool) -> Server {
        Server { key, nonce: [7; NONCE_LEN], writes, levels: 0 }
    }

    /// One request/answer round through both directions; the event the client ended with.
    fn exchange(client: &mut Client, server: &mut Server, now: u64) -> Option<Event> {
        let mut out = [0u8; FRAME_MAX];
        let (length, event) = client.tick(now, &mut out);
        assert!(event.is_none());
        assert!(length >= 60, "short frames are padded");
        let challenge = server.answer(&out[..length]);
        let (length, event) = client.receive(now, &challenge, &mut out);
        assert!(event.is_none() && length > 0);
        let reply = server.answer(&out[..length]);
        client.receive(now, &reply, &mut out).1
    }

    #[test]
    fn it_polls_the_state_and_writes() {
        let mut client = Client::new(ME, PI, b"correct horse").unwrap();
        let mut pi = server(b"correct horse", true);
        assert_eq!(exchange(&mut client, &mut pi, 0), Some(Event::State));
        let state = client.state.clone().unwrap();
        assert_eq!((state.function_of(7), state.pull_of(9), state.is_writable(5)), (1, 1, true));
        assert!(client.submit(Request::Set { mask: 1 << 5, values: 1 << 5 }));
        assert_eq!(exchange(&mut client, &mut pi, 10), Some(Event::Done(STATUS_OK)));
        assert_eq!(pi.levels, 1 << 5);
        // The next pass polls again at once.
        assert_eq!(exchange(&mut client, &mut pi, 20), Some(Event::State));
        assert!(client.state.as_ref().unwrap().level(5));
    }

    #[test]
    fn a_machine_that_does_not_take_writes_says_so() {
        let mut client = Client::new(ME, PI, b"k").unwrap();
        let mut pi = server(b"k", false);
        assert_eq!(exchange(&mut client, &mut pi, 0), Some(Event::State));
        assert!(!client.state.as_ref().unwrap().is_writable(5));
        client.submit(Request::Pull { pin: 5, pull: 1 });
        assert_eq!(exchange(&mut client, &mut pi, 5), Some(Event::Done(STATUS_NO_WRITE)));
    }

    #[test]
    fn an_answer_signed_with_another_key_is_refused() {
        let mut client = Client::new(ME, PI, b"battery staple").unwrap();
        let mut pi = server(b"correct horse", true);
        let mut out = [0u8; FRAME_MAX];
        let (length, _) = client.tick(0, &mut out);
        let challenge = pi.answer(&out[..length]);
        assert!(client.receive(0, &challenge, &mut out).0 > 0);
        // The real server drops our request (its MAC is wrong); an impostor that answers anyway is not believed.
        let mut forged = challenge.clone();
        forged.truncate(15);
        forged.push(STATUS_OK);
        forged.extend_from_slice(&pi.nonce);
        let data = [0u8; wire::STATE_DATA_LEN];
        forged.extend_from_slice(&data);
        forged.extend_from_slice(&wire::mac(REPLY_LABEL, STATUS_OK, &pi.nonce, &data, b"correct horse"));
        assert_eq!(client.receive(0, &forged, &mut out).1, Some(Event::Refused("the answer is not signed with our key")));
        assert!(!client.busy());
    }

    #[test]
    fn silence_times_out_and_the_next_poll_comes_later() {
        let mut client = Client::new(ME, PI, b"k").unwrap();
        let mut out = [0u8; FRAME_MAX];
        assert!(client.tick(0, &mut out).0 > 0);
        assert_eq!(client.tick(TIMEOUT_MS, &mut out), (0, None));
        assert_eq!(client.tick(TIMEOUT_MS + 1, &mut out), (0, Some(Event::Silent)));
        assert_eq!(client.tick(TIMEOUT_MS + 2, &mut out).0, 0);
        assert!(client.tick(TIMEOUT_MS + 1 + POLL_MS, &mut out).0 > 0);
    }

    #[test]
    fn frames_from_others_are_ignored() {
        let mut client = Client::new(ME, PI, b"k").unwrap();
        let mut pi = server(b"k", true);
        let mut out = [0u8; FRAME_MAX];
        let (length, _) = client.tick(0, &mut out);
        let mut challenge = pi.answer(&out[..length]);
        challenge[6] ^= 0xff;
        assert_eq!(client.receive(0, &challenge, &mut out), (0, None));
        assert!(client.busy());
    }
}
