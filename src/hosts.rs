//! Hosts: terminal apps whose terminals Trailboss can see, type into and read, behind the
//! four verbs of [`Host`]. Herdr's terminals survive a reboot (they come back as bare
//! shells); tmux's do with tmux-resurrect.

pub mod herdr;
pub mod tmux;

/// A terminal as a host lists it.
pub struct Listed {
    /// Stable across a restore: Herdr's `t5`, tmux's `work:2.1`.
    pub address: String,
    /// Where it is, for people: the space / tab, or the tmux window.
    pub location: Option<String>,
    /// The host's word for what's happening (Herdr: working, blocked, done).
    pub status: Option<String>,
    /// The program in front, when the listing says, so bare shells can be skipped cheaply.
    pub program: Option<String>,
}

/// One terminal, looked at closely.
#[derive(Default)]
pub struct Detail {
    /// The terminal's own process (usually a shell).
    pub shell_pid: Option<u32>,
    /// What's in front: the shell itself at a prompt, else the program it runs.
    pub foreground_pid: Option<u32>,
    /// When the terminal was made, if the host says (Herdr keeps it across a restore).
    pub created_ms: Option<i64>,
    /// Herdr sub-agent link: the parent terminal's address, and this one's name.
    pub parent: Option<String>,
    pub name: Option<String>,
}

pub trait Host {
    /// The key prefix in tracked.json and events: `herdr`, `tmux`.
    fn name(&self) -> &'static str;
    /// Every live terminal; None when the host isn't running.
    fn list(&self) -> Option<Vec<Listed>>;
    fn get(&self, address: &str) -> Option<Detail>;
    /// Type `text` into the terminal and press Enter.
    fn send(&self, address: &str, text: &str) -> Result<(), String>;
    /// The terminal's screen, as text.
    fn read(&self, address: &str) -> Option<String>;
}

pub fn all() -> Vec<Box<dyn Host>> {
    vec![Box::new(herdr::Herdr), Box::new(tmux::Tmux)]
}
