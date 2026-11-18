//! PAM authentication on a worker thread, so a slow `pam_faildelay` never
//! freezes the UI (and the compositor keeps getting frames).

use std::thread;

use smithay_client_toolkit::reexports::calloop::channel::Sender;
use zeroize::Zeroizing;

#[derive(Debug)]
pub enum AuthResult {
    Ok,
    Denied,
    /// PAM itself broke (missing service, module error...). Never unlocks.
    Error(String),
}

#[derive(Clone)]
pub enum Backend {
    Pam { service: String },
    /// `--demo`: password is literally "demo". Never used when actually locking.
    Demo,
}

impl Backend {
    pub fn pam() -> Self {
        // Ship /etc/pam.d/conchy-lock; fall back to `login` so a half-installed
        // system can still unlock.
        let service = if std::path::Path::new("/etc/pam.d/conchy-lock").exists() {
            "conchy-lock"
        } else {
            tracing::warn!("/etc/pam.d/conchy-lock missing, falling back to `login`");
            "login"
        };
        Backend::Pam { service: service.into() }
    }
}

pub fn spawn(
    backend: Backend,
    user: String,
    password: Zeroizing<String>,
    tx: Sender<AuthResult>,
) {
    thread::spawn(move || {
        let result = match backend {
            Backend::Demo => {
                thread::sleep(std::time::Duration::from_millis(700));
                if password.as_str() == "demo" {
                    AuthResult::Ok
                } else {
                    AuthResult::Denied
                }
            }
            Backend::Pam { service } => match pam::Client::with_password(&service) {
                Err(e) => AuthResult::Error(e.to_string()),
                Ok(mut client) => {
                    client.conversation_mut().set_credentials(&user, password.as_str());
                    match client.authenticate() {
                        Ok(()) => AuthResult::Ok,
                        Err(_) => AuthResult::Denied,
                    }
                }
            },
        };
        // `password` is zeroized on drop here.
        drop(password);
        let _ = tx.send(result);
    });
}
