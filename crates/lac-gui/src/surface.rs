//! MIDI input from control surfaces in Mackie Control mode (issue #95).
//!
//! What the messages mean is decided in [`lac_app::control_surface`]; this
//! module only connects to the system's MIDI inputs (ALSA, CoreMIDI or
//! Windows MIDI through `midir`) and hands the messages to the GUI thread.
//! It listens only while Play > Control surface is on, so a MIDI keyboard
//! cannot start playback by accident.
//!
//! Devices come and go: the inputs are listed again every
//! [`SCAN_INTERVAL`], new ones are connected and vanished ones dropped.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use lac_app::control_surface::{self, Command};
use lac_app::i18n::trf;
use midir::{Ignore, MidiInput, MidiInputConnection};

/// How often the MIDI inputs are listed again.
const SCAN_INTERVAL: Duration = Duration::from_secs(2);

/// The name this program gives itself towards the MIDI system.
const CLIENT: &str = "losslessaudiocut";

/// The connected MIDI inputs and the messages they sent.
pub struct Surfaces {
    tx: Sender<Vec<u8>>,
    rx: Receiver<Vec<u8>>,
    /// Each connected input with its name.
    connections: Vec<(String, MidiInputConnection<()>)>,
    /// When to list the inputs again; `None`: at the next poll.
    next_scan: Option<Instant>,
}

impl Default for Surfaces {
    fn default() -> Self {
        let (tx, rx) = channel();
        Surfaces {
            tx,
            rx,
            connections: Vec::new(),
            next_scan: None,
        }
    }
}

/// What [`Surfaces::poll`] found.
#[derive(Default)]
pub struct Polled {
    /// The commands received since the last poll, in order.
    pub commands: Vec<Command>,
    /// Inputs that were connected or disconnected, for the status bar.
    pub messages: Vec<String>,
}

impl Surfaces {
    /// Timer: connects to the MIDI inputs while `enabled` (disconnects
    /// otherwise) and returns what arrived.
    pub fn poll(&mut self, enabled: bool) -> Polled {
        let mut out = Polled::default();
        if !enabled {
            for (name, _) in self.connections.drain(..) {
                out.messages.push(trf(
                    "control surface disconnected: {name}",
                    &[("name", &name)],
                ));
            }
            self.next_scan = None;
            // Whatever arrived before switching off is dropped.
            while self.rx.try_recv().is_ok() {}
            return out;
        }
        if self.next_scan.is_none_or(|t| Instant::now() >= t) {
            self.scan(&mut out.messages);
            self.next_scan = Some(Instant::now() + SCAN_INTERVAL);
        }
        out.commands = self
            .rx
            .try_iter()
            .filter_map(|m| control_surface::parse(&m))
            .collect();
        out
    }

    /// Connects new inputs and drops vanished ones.
    fn scan(&mut self, messages: &mut Vec<String>) {
        // A system without MIDI (no ALSA sequencer, say) has no surfaces;
        // that is not worth a message every two seconds.
        let Ok(probe) = MidiInput::new(CLIENT) else {
            return;
        };
        let names: Vec<String> = probe
            .ports()
            .iter()
            .filter_map(|p| probe.port_name(p).ok())
            // ALSA's loopback port only echoes other programs' output.
            .filter(|n| !n.contains("Midi Through"))
            .collect();
        self.connections.retain(|(name, _)| {
            let present = names.contains(name);
            if !present {
                messages.push(trf(
                    "control surface disconnected: {name}",
                    &[("name", name)],
                ));
            }
            present
        });
        for name in names {
            if self.connections.iter().any(|(n, _)| *n == name) {
                continue;
            }
            if let Some(c) = self.connect(&name) {
                messages.push(trf("control surface connected: {name}", &[("name", &name)]));
                self.connections.push((name, c));
            }
        }
    }

    /// Opens the input called `name`; its messages go to the channel.
    fn connect(&self, name: &str) -> Option<MidiInputConnection<()>> {
        // `connect` consumes the client, so each input gets its own.
        let mut input = MidiInput::new(CLIENT).ok()?;
        // SysEx, clock and active sensing carry nothing for us.
        input.ignore(Ignore::All);
        let port = input
            .ports()
            .into_iter()
            .find(|p| input.port_name(p).ok().as_deref() == Some(name))?;
        let tx = self.tx.clone();
        input
            .connect(
                &port,
                CLIENT,
                move |_stamp, msg, _| {
                    // The receiver lives as long as the connection.
                    let _ = tx.send(msg.to_vec());
                },
                (),
            )
            .ok()
    }
}
