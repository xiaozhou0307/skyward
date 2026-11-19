//! The phone's WLAN inside the recovery, on a thread of its own: the driver
//! brought up, the phone's own wpa_supplicant joining networks, and an
//! address asked for over DHCP and kept. The screen is told how things
//! stand each time that changes.

use anyhow::{Context, Result, bail, ensure};
use skyward_config::{Firmware, Wlan as Config};
use skyward_net::dhcp::{self, Kept, Lease};
use skyward_net::http::Servers;
use skyward_net::iface;
use skyward_net::reach::{self, Reach};
use skyward_net::supplicant::{self, Bss, Control, Event as Told, Security};
use skyward_net::wlan::{self, Joined, Link, Next, Power, Stage, Traffic, Wlan};
use skyward_sys::worker::{self, Teller};
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::net::Ipv4Addr;
use std::os::fd::RawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// The first folder the firmware loader looks in that a recovery can
/// write.
const FIRMWARE: &str = "/etc/firmware";

/// Where a partition a firmware file is read from is mounted meanwhile.
const PARTITIONS: &str = "/skyward/wlan";

/// The supplicant's settings, and the folder of its control socket, in the
/// recovery service's own folder.
const HOME: &str = "/skyward/run/supplicant";

/// The user the supplicant takes on as it starts, who has to be able to
/// make its socket.
const WIFI: u32 = 1010;

/// The socket the phone's init makes for the supplicant and hands it, by the
/// name the phone's own init rc gives it.
const SUPPLICANT_SOCKET: &str = "wpa_wlan0";

/// Where init puts the sockets it makes.
const SOCKETS: &str = "/dev/socket";

/// How long the driver may take to make its interface, the chip's firmware
/// loading meanwhile.
const INTERFACE_WAIT: Duration = Duration::from_secs(20);

/// How long a helper the configuration runs once may take to end.
const HELPER_WAIT: Duration = Duration::from_secs(20);

/// How long the supplicant may take to open its control socket.
const SOCKET_WAIT: Duration = Duration::from_secs(10);

/// How long the supplicant gets to end once asked to.
const END_WAIT: Duration = Duration::from_secs(3);

/// How often a network joined is read for its signal.
const POLL: Duration = Duration::from_secs(5);

/// How often what went over the interface is counted, for how fast it
/// goes.
const TRAFFIC: Duration = Duration::from_secs(1);

/// How long after the network gave no address it is asked again.
const RETRY: Duration = Duration::from_secs(10);

/// What the screen asks of the WLAN.
pub enum Command {
    On,
    Off,
    Scan,
    Join {
        ssid: Vec<u8>,
        name: String,
        security: Security,
        password: String,
    },
    Leave,
}

/// What comes to the thread: a command, how asking for an address went,
/// how far the network joined reaches, or that the screen has let the
/// thread go.
enum Inbox {
    Command(Command),
    Address(u64, Result<Asked>),
    Reach(u64, Reach),
    End,
}

enum Asked {
    Got(Lease),
    Kept(Kept),
}

/// What the thread tells the screen: how the WLAN stands, or how fast data
/// went over it in the last second.
enum Report {
    Wlan(Wlan),
    Traffic(Traffic),
}

pub struct Worker {
    /// What the thread is asked, each of which wakes it.
    inbox: Teller<Inbox>,
    /// What the screen is told. The thread is waited for once it has been
    /// asked to end.
    worker: worker::Worker<Report>,
}

impl Worker {
    /// Start the thread, with the WLAN off. `modules` are the folders the
    /// driver's modules are found in, `libraries` the bundle of the vendor
    /// interface's libraries, and `servers` where the DNS servers the
    /// network gives are kept for whoever looks names up.
    pub fn start(
        cfg: Config,
        modules: Vec<PathBuf>,
        libraries: Option<PathBuf>,
        servers: Servers,
    ) -> Result<Worker> {
        let (inbox, asked) = worker::telling().context("make the WLAN thread's inbox")?;
        let back = inbox.clone();
        let worker = worker::Worker::start("wlan", move |tell| {
            let mut s = Station {
                cfg,
                modules,
                libraries,
                servers,
                state: Wlan::default(),
                running: None,
                back,
                tell,
            };
            s.run(&asked);
        })?;
        Ok(Worker { inbox, worker })
    }

    pub fn send(&self, c: Command) {
        self.inbox.tell(Inbox::Command(c));
    }

    /// Readable while the screen has something to be told.
    pub fn fd(&self) -> RawFd {
        self.worker.fd()
    }

    /// How the WLAN stands now, and how fast data went over it in the last
    /// second, each if told since the last call.
    pub fn told(&self) -> (Option<Wlan>, Option<Traffic>) {
        let (mut wlan, mut traffic) = (None, None);
        for report in self.worker.events() {
            match report {
                Report::Wlan(w) => wlan = Some(w),
                Report::Traffic(t) => traffic = Some(t),
            }
        }
        (wlan, traffic)
    }
}

impl Drop for Worker {
    /// The WLAN goes off with the thread.
    fn drop(&mut self) {
        self.inbox.tell(Inbox::End);
    }
}

/// The supplicant running, and the address the network gave.
struct Running {
    child: Child,
    control: Control,
    monitor: Control,
    mac: [u8; 6],
    found: Vec<Bss>,
    lease: Option<Held>,
    /// The thread asking for an address or to keep one, and its number;
    /// what an earlier one answers is let go.
    asking: Option<(u64, Arc<AtomicBool>)>,
    asked: u64,
    /// The number of the last look at how far the network reaches; what an
    /// earlier one finds is let go.
    probed: u64,
    /// When to ask for an address again after the network gave none.
    retry_at: Option<Instant>,
    polled_at: Instant,
    /// What had gone over the interface when last counted, while it has an
    /// address.
    counted: Option<Counted>,
}

/// The interface's totals in and out, in bytes, and when they were read.
#[derive(Clone, Copy)]
struct Counted {
    at: Instant,
    rx: u64,
    tx: u64,
}

/// A lease, when it was given, and how far keeping it has come: 0 asks its
/// server at the renewal time, 1 any server at the rebinding time, 2 lets
/// it go when it runs out.
struct Held {
    lease: Lease,
    at: Instant,
    phase: u8,
}

impl Held {
    fn due(&self) -> Instant {
        let after = match self.phase {
            0 => self.lease.renew,
            1 => self.lease.rebind,
            _ => self.lease.seconds,
        };
        self.at + Duration::from_secs(after.into())
    }
}

struct Station {
    cfg: Config,
    modules: Vec<PathBuf>,
    libraries: Option<PathBuf>,
    servers: Servers,
    state: Wlan,
    running: Option<Running>,
    /// What a thread asking for an address, or looking how far the
    /// network reaches, answers to: the thread's own inbox.
    back: Teller<Inbox>,
    tell: Teller<Report>,
}

impl Station {
    fn run(&mut self, asked: &worker::Told<Inbox>) {
        let mut told = self.state.clone();
        loop {
            let mut fds = vec![libc::pollfd {
                fd: asked.fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            if let Some(r) = &self.running {
                fds.push(libc::pollfd {
                    fd: r.monitor.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            let timeout = self.next().map_or(-1, |at| {
                at.saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(60_000) as i32
            });
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) } < 0 {
                let e = std::io::Error::last_os_error();
                assert_eq!(
                    e.kind(),
                    std::io::ErrorKind::Interrupted,
                    "poll the WLAN thread's inputs: {e}"
                );
            }
            for message in asked.take() {
                match message {
                    Inbox::Command(c) => self.command(c),
                    Inbox::Address(n, answer) => self.addressed(n, answer),
                    Inbox::Reach(n, reach) => self.reached(n, reach),
                    Inbox::End => return self.off(),
                }
            }
            self.listen();
            self.timers();
            if self.state != told {
                told = self.state.clone();
                self.tell.tell(Report::Wlan(told.clone()));
            }
        }
    }

    /// The soonest a timer is due.
    fn next(&self) -> Option<Instant> {
        let r = self.running.as_ref()?;
        let poll = matches!(
            self.state.joined,
            Some(Joined {
                stage: Stage::Connected(_),
                ..
            })
        )
        .then(|| r.polled_at + POLL);
        let count = r
            .lease
            .as_ref()
            .map(|_| r.counted.map_or_else(Instant::now, |c| c.at + TRAFFIC));
        [
            r.lease
                .as_ref()
                .filter(|_| r.asking.is_none())
                .map(Held::due),
            r.retry_at,
            poll,
            count,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn command(&mut self, c: Command) {
        match c {
            Command::On => self.on(),
            Command::Off => self.off(),
            Command::Scan => {
                if let Some(r) = &self.running {
                    // A scan under way answers FAIL-BUSY, and its results
                    // come all the same.
                    if let Err(e) = r.control.ask("SCAN") {
                        self.lost(e);
                    }
                }
            }
            Command::Join {
                ssid,
                name,
                security,
                password,
            } => {
                self.unaddress();
                let mut joined = Joined {
                    ssid,
                    name,
                    security,
                    stage: Stage::Joining,
                };
                if let Err(e) = self.join(&joined, &password) {
                    let _ = skyward_sys::kmsg(&format!("joining {} failed: {e:#}", joined.name));
                    joined.stage = Stage::Failed;
                }
                self.state.joined = Some(joined);
            }
            Command::Leave => {
                self.unaddress();
                if let Some(r) = &self.running
                    && let Err(e) = r.control.order("REMOVE_NETWORK all")
                {
                    self.lost(e);
                }
                self.state.joined = None;
            }
        }
    }

    fn on(&mut self) {
        if self.running.is_some() {
            return;
        }
        self.state.power = Power::Starting;
        self.tell.tell(Report::Wlan(self.state.clone()));
        match self.start() {
            Ok(r) => {
                self.running = Some(r);
                self.state.power = Power::On;
                self.command(Command::Scan);
            }
            Err(e) => {
                let _ = skyward_sys::kmsg(&format!("the WLAN did not come up: {e:#}"));
                self.state.power = Power::Failed(format!("{e:#}"));
            }
        }
    }

    fn start(&self) -> Result<Running> {
        bring_up(&self.cfg, &self.modules)?;
        let mut child = start_supplicant(&self.cfg, self.libraries.as_deref())?;
        let socket = Path::new(HOME).join("sockets").join(&self.cfg.interface);
        let connected = (|| -> Result<(Control, Control)> {
            let control = Control::open(&socket)?;
            ensure!(
                control.ask("PING")?.trim_end() == "PONG",
                "wpa_supplicant does not answer PING"
            );
            let monitor = Control::open(&socket)?;
            monitor.attach()?;
            Ok((control, monitor))
        })();
        let (control, monitor) = match connected {
            Ok(both) => both,
            Err(e) => {
                end(&mut child, None);
                return Err(e);
            }
        };
        let mac = iface::hardware_address(&self.cfg.interface)?;
        Ok(Running {
            child,
            control,
            monitor,
            mac,
            found: Vec::new(),
            lease: None,
            asking: None,
            asked: 0,
            probed: 0,
            retry_at: None,
            polled_at: Instant::now(),
            counted: None,
        })
    }

    fn off(&mut self) {
        self.unaddress();
        if let Some(mut r) = self.running.take() {
            end(&mut r.child, Some(&r.control));
        }
        if let Err(e) = iface::set_up(&self.cfg.interface, false) {
            let _ = skyward_sys::kmsg(&format!("{e:#}"));
        }
        self.state = Wlan::default();
    }

    /// The supplicant no longer answers: the WLAN is off, and why is said.
    fn lost(&mut self, e: anyhow::Error) {
        let _ = skyward_sys::kmsg(&format!("the WLAN went away: {e:#}"));
        self.off();
        self.state.power = Power::Failed(format!("{e:#}"));
    }

    fn join(&self, joined: &Joined, password: &str) -> Result<()> {
        let r = self.running.as_ref().context("the WLAN is off")?;
        r.control.order("REMOVE_NETWORK all")?;
        let id: u32 = r
            .control
            .ask("ADD_NETWORK")?
            .trim()
            .parse()
            .context("wpa_supplicant numbered the network with no number")?;
        // One the last scan did not see may hide its name, as one added by
        // hand may.
        let hidden = !r.found.iter().any(|b| b.ssid == joined.ssid);
        for c in supplicant::network_commands(id, &joined.ssid, joined.security, password, hidden)?
        {
            r.control.order(&c)?;
        }
        r.control.order(&format!("SELECT_NETWORK {id}"))
    }

    /// Read what the supplicant has told.
    fn listen(&mut self) {
        let Some(r) = &self.running else { return };
        let messages = match r.monitor.messages() {
            Ok(m) => m,
            Err(e) => return self.lost(e),
        };
        for m in messages {
            let Some(told) = supplicant::event(&m) else {
                continue;
            };
            match told {
                Told::ScanResults => {
                    if let Err(e) = self.read_scan() {
                        return self.lost(e);
                    }
                }
                Told::ScanFailed => {}
                Told::Terminating => return self.lost(anyhow::anyhow!("wpa_supplicant ended")),
                other => self.joining(&other),
            }
        }
    }

    fn read_scan(&mut self) -> Result<()> {
        let r = self
            .running
            .as_mut()
            .expect("scan results come from a supplicant running");
        let mut found = Vec::new();
        let mut first = 0;
        loop {
            let page = supplicant::bss_list(&r.control.ask(&supplicant::bss_command(first))?)?;
            let Some(last) = page.last() else { break };
            first = last.id + 1;
            found.extend(page);
        }
        self.state.networks = wlan::networks(&found);
        r.found = found;
        Ok(())
    }

    fn joining(&mut self, told: &Told) {
        let Some(j) = &mut self.state.joined else {
            return;
        };
        match j.on(told) {
            Next::Show => {}
            Next::Address => self.ask_address(),
            Next::Unaddress => self.unaddress(),
            Next::Forget => {
                if let Some(r) = &self.running
                    && let Err(e) = r.control.order("REMOVE_NETWORK all")
                {
                    self.lost(e);
                }
            }
        }
    }

    /// Ask the network for an address, on a thread of its own.
    fn ask_address(&mut self) {
        let interface = self.cfg.interface.clone();
        let back = self.back.clone();
        let Some(r) = &mut self.running else { return };
        r.retry_at = None;
        if let Some((_, stop)) = r.asking.take() {
            stop.store(true, Ordering::Relaxed);
        }
        r.asked += 1;
        let n = r.asked;
        let stop = Arc::new(AtomicBool::new(false));
        r.asking = Some((n, stop.clone()));
        let mac = r.mac;
        let keeping = r.lease.as_ref().map(|h| (h.lease.clone(), h.phase));
        thread::spawn(move || {
            let answer = match keeping {
                None => dhcp::obtain(&interface, mac, &stop).map(Asked::Got),
                Some((lease, phase)) => {
                    dhcp::keep(&interface, mac, &lease, phase > 0, &stop).map(Asked::Kept)
                }
            };
            back.tell(Inbox::Address(n, answer));
        });
    }

    fn addressed(&mut self, n: u64, answer: Result<Asked>) {
        let Some(r) = &mut self.running else { return };
        if r.asking.as_ref().map(|(m, _)| *m) != Some(n) {
            return;
        }
        r.asking = None;
        let lease = match answer {
            Ok(Asked::Got(lease)) | Ok(Asked::Kept(Kept::Kept(lease))) => lease,
            Ok(Asked::Kept(Kept::Unanswered)) => {
                if let Some(h) = &mut r.lease {
                    h.phase += 1;
                }
                return;
            }
            Ok(Asked::Kept(Kept::Refused)) => {
                self.unaddress();
                return self.ask_address();
            }
            Err(e) if e.is::<skyward_net::Stopped>() => return,
            Err(e) => {
                let _ = skyward_sys::kmsg(&format!("no address: {e:#}"));
                r.retry_at = Some(Instant::now() + RETRY);
                if let Some(j) = &mut self.state.joined {
                    j.stage = Stage::NoAddress(format!("{e:#}"));
                }
                return;
            }
        };
        let same = r.lease.as_ref().is_some_and(|h| {
            (h.lease.address, h.lease.mask, h.lease.router)
                == (lease.address, lease.mask, lease.router)
        });
        if !same
            && let Err(e) =
                iface::configure(&self.cfg.interface, lease.address, lease.mask, lease.router)
        {
            let _ = skyward_sys::kmsg(&format!("{e:#}"));
            r.retry_at = Some(Instant::now() + RETRY);
            if let Some(j) = &mut self.state.joined {
                j.stage = Stage::NoAddress(format!("{e:#}"));
            }
            return;
        }
        let _ = skyward_sys::kmsg(&format!(
            "{} has {} ({}) through {:?} for {} s, kept again after {} s",
            self.cfg.interface, lease.address, lease.mask, lease.router, lease.seconds, lease.renew
        ));
        *self
            .servers
            .lock()
            .expect("the DNS servers are not held by a thread that panicked") = lease.dns.clone();
        let mut link = Link::new(&lease, r.mac);
        // An address kept keeps how far its network was found to reach; a
        // new one has it looked at.
        link.reach = match &self.state.joined {
            Some(Joined {
                stage: Stage::Connected(had),
                ..
            }) if same => had.reach.clone(),
            _ => None,
        };
        let look = link.reach.is_none().then(|| lease.dns.clone());
        r.lease = Some(Held {
            lease,
            at: Instant::now(),
            phase: 0,
        });
        if let Some(j) = &mut self.state.joined {
            j.stage = Stage::Connected(link);
        }
        if let Some(dns) = look {
            self.probe(dns);
        }
        self.poll_link();
    }

    /// Look how far the network joined reaches, on a thread of its own, the
    /// page's name looked up in the network's own DNS servers, `dns`.
    fn probe(&mut self, dns: Vec<Ipv4Addr>) {
        let (page, back) = (self.cfg.probe.clone(), self.back.clone());
        let Some(r) = &mut self.running else { return };
        r.probed += 1;
        let n = r.probed;
        thread::spawn(move || {
            let reach = reach::probe(&page, &dns);
            back.tell(Inbox::Reach(n, reach));
        });
    }

    fn reached(&mut self, n: u64, reach: Reach) {
        if self.running.as_ref().is_none_or(|r| r.probed != n) {
            return;
        }
        let Some(Joined {
            stage: Stage::Connected(link),
            ..
        }) = &mut self.state.joined
        else {
            return;
        };
        let _ = skyward_sys::kmsg(&format!("the network on {} {reach}", self.cfg.interface));
        link.reach = Some(reach);
    }

    /// Give the address back, and stop asking for one or keeping it.
    fn unaddress(&mut self) {
        let Some(r) = &mut self.running else { return };
        r.retry_at = None;
        if let Some((_, stop)) = r.asking.take() {
            stop.store(true, Ordering::Relaxed);
        }
        if r.lease.take().is_some()
            && let Err(e) = iface::unconfigure(&self.cfg.interface)
        {
            let _ = skyward_sys::kmsg(&format!("{e:#}"));
        }
        // Nothing goes over a network left, and counting starts afresh on
        // the next one.
        if r.counted.take().is_some() {
            self.tell.tell(Report::Traffic(Traffic::default()));
        }
        self.servers
            .lock()
            .expect("the DNS servers are not held by a thread that panicked")
            .clear();
    }

    fn timers(&mut self) {
        let now = Instant::now();
        let Some(r) = &mut self.running else { return };
        if r.retry_at.is_some_and(|at| now >= at) {
            r.retry_at = None;
            return self.ask_address();
        }
        if r.asking.is_none()
            && let Some(h) = &r.lease
            && now >= h.due()
        {
            if h.phase >= 2 {
                // The lease ran out with no server to keep it.
                self.unaddress();
                if let Some(j) = &mut self.state.joined {
                    j.stage = Stage::Addressing;
                }
            }
            return self.ask_address();
        }
        if now >= r.polled_at + POLL {
            self.poll_link();
        }
        if let Some(r) = &self.running
            && r.lease.is_some()
            && r.counted.is_none_or(|c| now >= c.at + TRAFFIC)
        {
            self.count(now);
        }
    }

    /// Read what has gone over the interface, and tell how fast it went
    /// since the last reading.
    fn count(&mut self, now: Instant) {
        let stats = Path::new("/sys/class/net")
            .join(&self.cfg.interface)
            .join("statistics");
        let read = |name: &str| -> Result<u64> {
            let path = stats.join(name);
            let text =
                fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
            text.trim()
                .parse()
                .with_context(|| format!("{} holds no count", path.display()))
        };
        let (rx, tx) = match (read("rx_bytes"), read("tx_bytes")) {
            (Ok(rx), Ok(tx)) => (rx, tx),
            (Err(e), _) | (_, Err(e)) => return self.lost(e),
        };
        let Some(r) = &mut self.running else { return };
        if let Some(c) = r.counted {
            let secs = now.duration_since(c.at).as_secs_f64();
            let rate = |total: u64, before: u64| {
                (total.saturating_sub(before) as f64 / secs).round() as u64
            };
            self.tell.tell(Report::Traffic(Traffic {
                rx: rate(rx, c.rx),
                tx: rate(tx, c.tx),
            }));
        }
        r.counted = Some(Counted { at: now, rx, tx });
    }

    /// Read the network joined for its signal and speed.
    fn poll_link(&mut self) {
        let Some(r) = &mut self.running else { return };
        r.polled_at = Instant::now();
        let Some(Joined {
            stage: Stage::Connected(link),
            ..
        }) = &mut self.state.joined
        else {
            return;
        };
        // A plain STATUS has Android's supplicant tell every listener again
        // that the network is connected; this one leaves that out.
        let (signal, status) = match (
            r.control.ask("SIGNAL_POLL"),
            r.control.ask("STATUS-NO_EVENTS"),
        ) {
            (Ok(signal), Ok(status)) => (signal, status),
            (Err(e), _) | (_, Err(e)) => return self.lost(e),
        };
        let fields = supplicant::fields(&signal);
        if let Some(rssi) = fields.get("RSSI").and_then(|v| v.parse().ok()) {
            link.signal = rssi;
        }
        if let Some(f) = fields.get("FREQUENCY").and_then(|v| v.parse().ok()) {
            link.frequency = f;
        }
        link.speed = fields.get("LINKSPEED").and_then(|v| v.parse().ok());
        link.generation = supplicant::fields(&status)
            .get("wifi_generation")
            .and_then(|v| v.parse().ok());
    }
}

/// How many times the modules that failed are tried again, once the module
/// holding what they said they could not find has been loaded.
const RETRIES: usize = 4;

/// Load the driver where its interface is not there yet, and bring the
/// interface up.
/// The nodes the vendor programs a WLAN start needs appear only once the WLAN
/// modules are in. Recovery's own ueventd read the vendor's rules before the
/// vendor partition was mounted, so each node still carries the kernel's own
/// owner. Give them the owner the phone's rules give them, and check it held.
fn prepare_control_nodes(nodes: &[String]) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let known = skyward_sys::ueventd::accounts(
        &skyward_sys::ueventd::PASSWD_FILES
            .iter()
            .chain(skyward_sys::ueventd::GROUP_FILES.iter())
            .copied()
            .collect::<Vec<_>>(),
    );
    let mut rules = Vec::new();
    for file in skyward_sys::ueventd::RULE_FILES {
        if let Ok(text) = fs::read_to_string(file) {
            rules.extend(skyward_sys::ueventd::rules(&text, &known));
        }
    }
    for node in nodes {
        let path = Path::new(node);
        let until = Instant::now() + Duration::from_secs(3);
        while !path.exists() && Instant::now() < until {
            thread::sleep(Duration::from_millis(50));
        }
        let Ok(before) = fs::symlink_metadata(path) else {
            let _ = skyward_sys::kmsg(&format!(
                "WLAN node {node}: not there once the modules were in, nothing given it"
            ));
            continue;
        };
        ensure!(before.file_type().is_char_device(), "{node} is not a character device");
        let Some(rule) = rules.iter().rev().find(|r| r.matches(node)) else {
            let _ = skyward_sys::kmsg(&format!(
                "WLAN node {node}: the phone's rules say nothing of it, left as it is"
            ));
            continue;
        };
        std::os::unix::fs::lchown(path, Some(rule.uid), Some(rule.gid))
            .with_context(|| format!("give {node} the owner the phone gives it"))?;
        fs::set_permissions(path, fs::Permissions::from_mode(rule.mode))
            .with_context(|| format!("give {node} the mode the phone gives it"))?;
        let after = fs::symlink_metadata(path)?;
        ensure!(
            after.file_type().is_char_device()
                && after.rdev() == before.rdev()
                && after.uid() == rule.uid
                && after.gid() == rule.gid
                && after.mode() & 0o777 == rule.mode,
            "{node} does not carry the owner the phone gives it"
        );
        let _ = skyward_sys::kmsg(&format!(
            "WLAN node {node}: {}:{} {:o} as the phone's rules say",
            rule.uid, rule.gid, rule.mode
        ));
    }
    Ok(())
}

fn bring_up(cfg: &Config, modules: &[PathBuf]) -> Result<()> {
    let net = Path::new("/sys/class/net").join(&cfg.interface);
    if !net.exists() {
        for file in &cfg.firmware {
            place(file)?;
        }
        // A phone keeps the modules of one kernel in several places, and the
        // module a driver needs first may name a symbol another one holds: a
        // kernel that cannot resolve a symbol refuses the module with "No
        // such file or directory", which says nothing of what was missing.
        // So the kernel's own log is read for the names it could not
        // resolve, the modules that hold them are looked for among the same
        // places, and the ones that failed are tried again.
        // Whatever lets the kernel take these modules has to be in place
        // before the first of them is handed over, and taken away as soon as
        // the last is in, so nothing else is loaded under it.
        if let Some(hook) = &cfg.hook {
            match std::fs::write(hook, "1") {
                Ok(()) => {
                    let _ = skyward_sys::kmsg(&format!("module hook: armed through {hook}"));
                }
                Err(e) => {
                    let _ = skyward_sys::kmsg(&format!("module hook: {hook} was not armed: {e}"));
                }
            }
        }
        let mut supplied: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut pending = cfg.modules.clone();
        let mut refusals: Vec<String> = Vec::new();
        for round in 0..=RETRIES {
            for path in skyward_modules::load_relaxed(modules, &pending, &mut |why| {
                let _ = skyward_sys::kmsg(&format!("module: {why}"));
                if why.starts_with("could not load ") {
                    refusals.push(why);
                }
            }) {
                skyward_sys::kmsg(&format!("loaded {}", path.display()))?;
            }
            if net.exists() || round == RETRIES {
                break;
            }
            // A module the kernel refused because of the symbols it holds is
            // refused every time: asking again, and reading a partition of
            // modules to find what it needs, only keeps the page waiting.
            if refusals.iter().any(|r| r.contains("Permission denied")) {
                break;
            }
            let said = kernel_log();
            let missing: std::collections::BTreeSet<String> =
                skyward_modules::unknown_symbols(&said).difference(&supplied).cloned().collect();
            if missing.is_empty() {
                break;
            }
            let found = skyward_modules::providing(modules, &missing);
            if found.is_empty() {
                skyward_sys::kmsg(&format!(
                    "no module here holds {} which the kernel could not resolve",
                    missing.iter().cloned().collect::<Vec<_>>().join(", ")
                ))?;
                break;
            }
            for (path, symbol) in found {
                skyward_sys::kmsg(&format!("{symbol} is held by {}", path.display()))?;
                match skyward_modules::insmod(&path, "") {
                    Ok(()) => {
                        supplied.insert(symbol);
                        skyward_sys::kmsg(&format!("loaded {}", path.display()))?;
                    }
                    Err(e) => skyward_sys::kmsg(&format!("could not load {}: {e:#}", path.display()))?,
                }
            }
            pending = cfg.modules.clone();
        }
        if let Some(hook) = &cfg.hook {
            let _ = std::fs::write(hook, "0");
            let _ = skyward_sys::kmsg(&format!("module hook: disarmed through {hook}"));
        }
        prepare_control_nodes(&cfg.nodes)?;
        for helper in &cfg.helper {
            // A helper that says what it leaves behind is asked for that, not
            // for how it ended: one that dies of its own fault after leaving it
            // has still done what the WLAN start needed, and one already left by
            // this boot is not asked again, as the vendor's own init asks it
            // once.
            if let Some(name) = &helper.ready_property
                && helper.leaves_ready_value(skyward_sys::prop(name).as_deref())
            {
                let _ = skyward_sys::kmsg(&format!(
                    "WLAN helper {}: {name} is already there, not started again",
                    helper.path
                ));
                continue;
            }
            let _ = skyward_sys::kmsg(&format!("WLAN helper: starting {}", helper.path));
            let program = skyward_programs::secure::Started::from(helper.clone());
            if helper.oneshot {
                // Stock runs these oneshot at boot and goes on once they end.
                // Wait for that, but never longer than a fixed bound, so a
                // helper that hangs is said in words rather than left to keep
                // the page saying the WLAN is turning on.
                let (done, ended) = std::sync::mpsc::channel();
                thread::Builder::new()
                    .name("wlan-helper".into())
                    .spawn(move || {
                        let _ = done.send(crate::services::once(&program));
                    })
                    .context("start a WLAN helper thread")?;
                match ended.recv_timeout(HELPER_WAIT) {
                    Ok(Ok(status)) if status.success() => {}
                    Ok(Ok(status)) if helper.ready_property.is_some() => {
                        let _ = skyward_sys::kmsg(&format!(
                            "WLAN helper {} ended {status}; asking what it leaves behind",
                            helper.path
                        ));
                    }
                    Ok(Ok(status)) => {
                        bail!("the WLAN helper {} ended {status}", helper.path)
                    }
                    Ok(Err(e)) => return Err(e).context("run a WLAN helper"),
                    Err(_) => bail!(
                        "the WLAN helper {} did not finish in {} seconds",
                        helper.path,
                        HELPER_WAIT.as_secs()
                    ),
                }
                let _ = skyward_sys::kmsg(&format!("WLAN helper: {} ended", helper.path));
            } else {
                crate::services::start(&[program], None).context("start a WLAN helper")?;
            }
            if let Some(name) = &helper.ready_property {
                let ready_wait = if helper.ready_timeout_ms == 0 { HELPER_WAIT } else {
                    Duration::from_millis(helper.ready_timeout_ms)
                };
                let until = Instant::now() + ready_wait;
                while !helper.leaves_ready_value(skyward_sys::prop(name).as_deref()) {
                    ensure!(
                        Instant::now() < until,
                        "the WLAN helper {} set no {name} in {} seconds",
                        helper.path,
                        ready_wait.as_secs()
                    );
                    thread::sleep(Duration::from_millis(100));
                }
                let _ = skyward_sys::kmsg(&format!("WLAN helper {} left {name} set", helper.path));
            }
        }
        for w in &cfg.writes {
            // ueventd may still be creating a character device after insmod.
            // Never create a regular file in its place (fs::write does).
            let path = Path::new(&w.path);
            let until = Instant::now() + Duration::from_secs(3);
            while !path.exists() && Instant::now() < until {
                thread::sleep(Duration::from_millis(50));
            }
            let metadata = fs::metadata(path)
                .with_context(|| format!("WLAN control node {} did not appear", w.path))?;
            if path.starts_with("/dev") {
                ensure!(metadata.file_type().is_char_device(),
                    "WLAN control node {} is not a character device", w.path);
            }
            // This MTK protocol requires wlan_assistant to deliver NVRAM first.
            // The stock helper sets this property only after the driver's write succeeds.
            if w.path == "/dev/wmtWifi" && w.value == "1\0" {
                let _ = skyward_sys::kmsg("WLAN control: waiting for stock NVRAM helper");
                let until = Instant::now() + Duration::from_secs(20);
                while skyward_sys::prop("vendor.mtk.nvram.ready").as_deref() != Some("1") {
                    ensure!(Instant::now() < until,
                        "stock wlan_assistant did not report vendor.mtk.nvram.ready=1 in 20 seconds; WLAN calibration/callback registration is not ready");
                    thread::sleep(Duration::from_millis(100));
                }
                let _ = skyward_sys::kmsg("WLAN control: stock NVRAM helper ready");
            }
            let mut node = fs::OpenOptions::new().write(true).open(path)
                .with_context(|| format!("open WLAN control node {}", w.path))?;
            let _ = skyward_sys::kmsg(&format!("WLAN control: writing {} bytes to {}", w.value.len(), w.path));
            node.write_all(w.value.as_bytes())
                .with_context(|| format!("write WLAN control node {}", w.path))?;
            let _ = skyward_sys::kmsg(&format!("WLAN control: write to {} completed", w.path));
        }
        // A driver the kernel refused outright makes no interface, and
        // waiting for one that will not come only keeps the page saying it
        // is turning on: say what the kernel said instead.
        if !net.exists() && !refusals.is_empty() {
            // A kernel that keeps symbols for itself refuses the modules that
            // hold them, whatever way they are handed over, and nothing this
            // side does changes it: the page says so rather than asking again.
            let protected = refusals.iter().any(|r| r.contains("Permission denied"));
            let why = refusals.join("; ");
            bail!(
                "{}{why}",
                if protected {
                    "the kernel here keeps some of its symbols for itself and refuses any module that holds one — \"exports protected symbol\" or \"Protected symbol … (err -13)\" in its own log. WiFi cannot start while that stands; the phone's own system boots with the same kernel and gets round it another way. "
                } else {
                    ""
                }
            );
        }
        let until = Instant::now() + INTERFACE_WAIT;
        while !net.exists() {
            ensure!(
                Instant::now() < until,
                "the driver made no {} in {} seconds",
                cfg.interface,
                INTERFACE_WAIT.as_secs()
            );
            thread::sleep(Duration::from_millis(100));
        }
    }
    iface::set_up(&cfg.interface, true)
}

/// What the kernel still holds of its log, and nothing on a machine that has
/// no kernel log of its own to read.
fn kernel_log() -> String {
    #[cfg(target_os = "android")]
    {
        match skyward_sys::kernel_log() {
            Ok(said) => return said,
            Err(e) => {
                let _ = skyward_sys::kmsg(&format!(
                    "the kernel log is not readable, so what a module needed is not known: {e:#}"
                ));
            }
        }
    }
    String::new()
}

/// Put a file the driver asks for where the firmware loader looks.
fn place(file: &Firmware) -> Result<()> {
    let to = Path::new(FIRMWARE).join(&file.name);
    let dir = to.parent().expect("a firmware file is in a folder");
    fs::create_dir_all(dir).with_context(|| format!("make {}", dir.display()))?;
    let bytes = match &file.partition {
        None => fs::read(&file.path).with_context(|| format!("read {}", file.path))?,
        Some(partition) => {
            let at = Path::new(PARTITIONS).join(partition);
            fs::create_dir_all(&at).with_context(|| format!("make {}", at.display()))?;
            skyward_block::mount::mount_ro(
                &Path::new(skyward_block::BY_NAME).join(partition),
                &at,
            )?;
            let read = fs::read(at.join(&file.path))
                .with_context(|| format!("read {} on {partition}", file.path));
            skyward_block::mount::umount(&at)?;
            read?
        }
    };
    fs::write(&to, bytes).with_context(|| format!("write {}", to.display()))
}

/// Make the socket the phone's own init makes for the supplicant: a datagram
/// socket bound at its name under /dev/socket, given to the account the
/// supplicant runs as, its number left in the variable init leaves it in.
/// This generation of supplicant asks for that socket and ends without a word
/// when there is none, which is what the kernel log showed:
/// "android_get_control_socket(wpa_wlan0) = -1" and then exit 255.
fn control_socket(name: &str) -> Result<RawFd> {
    let dir = Path::new(SOCKETS);
    fs::create_dir_all(dir).with_context(|| format!("make {}", dir.display()))?;
    let path = dir.join(name);
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0) };
    ensure!(
        fd >= 0,
        "make the socket {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("take away {}", path.display())),
    }
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    ensure!(
        bytes.len() < address.sun_path.len(),
        "{} is too long a name for a socket",
        path.display()
    );
    for (to, from) in address.sun_path.iter_mut().zip(bytes) {
        *to = *from as libc::c_char;
    }
    let size = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    let bound = unsafe { libc::bind(fd, (&address as *const libc::sockaddr_un).cast(), size) };
    ensure!(
        bound == 0,
        "bind {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    let _ = std::os::unix::fs::lchown(&path, Some(WIFI), Some(WIFI));
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o660));
    Ok(fd)
}

/// Start the supplicant on the interface, and wait for its control socket.
fn start_supplicant(cfg: &Config, libraries: Option<&Path>) -> Result<Child> {
    let home = Path::new(HOME);
    let sockets = home.join("sockets");
    fs::create_dir_all(&sockets).with_context(|| format!("make {}", sockets.display()))?;
    fs::set_permissions(home, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("open {} to the supplicant", home.display()))?;
    std::os::unix::fs::chown(&sockets, Some(WIFI), Some(WIFI))
        .with_context(|| format!("give {} to the supplicant", sockets.display()))?;
    fs::set_permissions(&sockets, fs::Permissions::from_mode(0o770))
        .with_context(|| format!("keep {} to the supplicant", sockets.display()))?;
    let settings = home.join("wpa_supplicant.conf");
    fs::write(
        &settings,
        format!(
            "ctrl_interface={}\nupdate_config=0\np2p_disabled=1\npmf=1\nsae_pwe=2\n",
            sockets.display()
        ),
    )
    .with_context(|| format!("write {}", settings.display()))?;
    fs::set_permissions(&settings, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("open {} to the supplicant", settings.display()))?;
    let socket = sockets.join(&cfg.interface);
    // One an earlier run left would answer nothing.
    if socket.exists() {
        fs::remove_file(&socket).with_context(|| format!("take away {}", socket.display()))?;
    }
    let mut path: Vec<String> = cfg.libraries.clone();
    if let Some(dir) = libraries {
        path.push(dir.to_string_lossy().into_owned());
    }
    // The socket the phone's init would have made before handing the program
    // over, made here and handed over the same way.
    let control = control_socket(SUPPLICANT_SOCKET)?;
    let mut command = Process::new(&cfg.supplicant);
    // The log bridge goes into the supplicant's scope because an Android
    // supplicant writes its warnings and errors through liblog rather than to
    // the streams, so a failed start would say no more than its exit status.
    // Which libraries it links against is settled by the configured folders,
    // not here: forcing single libraries in one at a time left the vendor's
    // copies of the rest in the way, and each one missing symbol only showed
    // once the one before it was satisfied.
    let mut preloads: Vec<String> = Vec::new();
    for candidate in ["/skyward/libkmsglog.so", "/skyward/libwpaaidl.so"] {
        if Path::new(candidate).is_file() {
            preloads.push(candidate.to_string());
        }
    }
    if !preloads.is_empty() {
        command.env("LD_PRELOAD", preloads.join(":"));
    }
    command.env(format!("ANDROID_SOCKET_{SUPPLICANT_SOCKET}"), control.to_string());
    let mut child = command
        .args(["-i", &cfg.interface, "-D", "nl80211", "-c"])
        .arg(&settings)
        // The phone's own init gives the supplicant its control-socket folder
        // this way ("-O/data/vendor/wifi/wpa/sockets"), and the supplicant of
        // this Android generation ends without a word when it is not told.
        .arg("-O")
        .arg(&sockets)
        // The global control socket the phone's own init names next to the
        // per-interface folder.
        .arg("-g@android:wpa_wlan0")
        .env("LD_LIBRARY_PATH", path.join(":"))
        .current_dir("/")
        .stdin(Stdio::null())
        // Keep startup errors: exit code 1 alone does not distinguish a
        // linker failure, an invalid config, or nl80211 initialization.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("start {}", cfg.supplicant))?;
    // Drain both pipes concurrently so a full pipe cannot block startup.
    // Do not enable key/debug dumps; normal diagnostics suffice here.
    fn relay(reader: impl std::io::Read + Send + 'static, stream: &'static str) {
        thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            let mut input = BufReader::new(reader);
            let mut line = Vec::new();
            while let Ok(bytes) = input.read_until(b'\n', &mut line) {
                if bytes == 0 { break; }
                let text = String::from_utf8_lossy(&line);
                let _ = skyward_sys::kmsg(&format!("wpa_supplicant {stream}: {}", text.trim_end()));
                line.clear();
            }
        });
    }
    if let Some(stdout) = child.stdout.take() { relay(stdout, "stdout"); }
    if let Some(stderr) = child.stderr.take() { relay(stderr, "stderr"); }
    let until = Instant::now() + SOCKET_WAIT;
    while !socket.exists() {
        if let Some(status) = child.try_wait().context("look at wpa_supplicant")? {
            bail!("wpa_supplicant ended as it started: {status}");
        }
        if Instant::now() >= until {
            end(&mut child, None);
            bail!(
                "wpa_supplicant opened no control socket in {} seconds",
                SOCKET_WAIT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(child)
}

/// End the supplicant: asked to, where it can be, and killed after a while.
fn end(child: &mut Child, control: Option<&Control>) {
    if let Some(c) = control {
        let _ = c.ask("TERMINATE");
        let until = Instant::now() + END_WAIT;
        while Instant::now() < until {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}
