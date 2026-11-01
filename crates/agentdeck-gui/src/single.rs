//! One GUI per user session. The first instance binds a unix socket in
//! `$XDG_RUNTIME_DIR`; a later launch connects, hands over its Wayland
//! activation token and exits, and the first instance raises its window.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use iced::{Subscription, Task};

/// The bound listener, taken by the subscription once the event loop runs.
static LISTENER: Mutex<Option<UnixListener>> = Mutex::new(None);

pub enum Claim {
    /// This process is the instance; the listener waits for later launches.
    Primary,
    /// Another instance was told to show itself.
    Handed,
}

fn socket_path() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("agentdeck");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("gui.sock"))
}

/// Without a runtime dir every launch is its own instance.
pub fn claim() -> Claim {
    let Some(path) = socket_path() else {
        return Claim::Primary;
    };
    match claim_at(
        &path,
        &std::env::var("XDG_ACTIVATION_TOKEN").unwrap_or_default(),
    ) {
        Ok(Some(listener)) => {
            *LISTENER.lock().unwrap_or_else(|e| e.into_inner()) = Some(listener);
            Claim::Primary
        }
        Ok(None) => Claim::Handed,
        // An unusable socket must not keep the GUI from opening.
        Err(_) => Claim::Primary,
    }
}

/// `Some(listener)` when this process is the instance; `None` after handing
/// `token` to the live one.
fn claim_at(path: &Path, token: &str) -> std::io::Result<Option<UnixListener>> {
    for _ in 0..2 {
        match UnixListener::bind(path) {
            Ok(l) => return Ok(Some(l)),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {}
            Err(e) => return Err(e),
        }
        match UnixStream::connect(path) {
            Ok(mut s) => {
                s.write_all(format!("show {token}\n").as_bytes())?;
                return Ok(None);
            }
            // Left by a crashed instance: nobody listens, take it over.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                std::fs::remove_file(path)?;
            }
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("instance socket keeps reappearing"))
}

/// Each later launch, as its activation token (empty when it had none).
pub fn subscription() -> Subscription<String> {
    Subscription::run(|| {
        iced::stream::channel(4, async |mut out| {
            use iced::futures::SinkExt;
            let Some(std_listener) = LISTENER.lock().unwrap_or_else(|e| e.into_inner()).take()
            else {
                return;
            };
            if std_listener.set_nonblocking(true).is_err() {
                return;
            }
            let Ok(listener) = tokio::net::UnixListener::from_std(std_listener) else {
                return;
            };
            while let Ok((stream, _)) = listener.accept().await {
                let Ok(stream) = stream.into_std() else {
                    continue;
                };
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(1)));
                let mut line = String::new();
                let _ = stream.take(512).read_to_string(&mut line);
                if let Some(token) = line.trim_end().strip_prefix("show") {
                    let _ = out.send(token.trim().to_string()).await;
                }
            }
        })
    })
}

/// Raise the window. Wayland grants focus only with the launcher's activation
/// token; without one the compositor just marks the window as wanting attention.
pub fn raise<T: Send + 'static>(token: String) -> Task<T> {
    iced::window::oldest().then(move |id| {
        let Some(id) = id else {
            return Task::none();
        };
        let token = token.clone();
        Task::batch([
            iced::window::minimize(id, false),
            iced::window::gain_focus(id),
            iced::window::run(id, move |w| wayland_activate(w, &token)).then(move |done| {
                if done {
                    Task::none()
                } else {
                    iced::window::request_user_attention(
                        id,
                        Some(iced::window::UserAttention::Informational),
                    )
                }
            }),
        ])
    })
}

/// `xdg_activation_v1.activate(token, surface)` on the window's own surface.
/// False when not on Wayland, without a token, or without the protocol.
fn wayland_activate(w: &dyn iced::window::Window, token: &str) -> bool {
    use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
    use wayland_client::protocol::{wl_registry, wl_surface::WlSurface};
    use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, globals};
    use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;

    struct Noop;
    impl Dispatch<wl_registry::WlRegistry, globals::GlobalListContents> for Noop {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &globals::GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    impl Dispatch<XdgActivationV1, ()> for Noop {
        fn event(
            _: &mut Self,
            _: &XdgActivationV1,
            _: <XdgActivationV1 as Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    if token.is_empty() {
        return false;
    }
    let (Ok(wh), Ok(dh)) = (w.window_handle(), w.display_handle()) else {
        return false;
    };
    let (RawWindowHandle::Wayland(wh), RawDisplayHandle::Wayland(dh)) = (wh.as_raw(), dh.as_raw())
    else {
        return false;
    };
    // SAFETY: winit owns both pointers and keeps them alive while this
    // callback runs on its event-loop thread; the foreign display is not closed here.
    let backend = unsafe {
        wayland_backend::client::Backend::from_foreign_display(dh.display.as_ptr().cast())
    };
    let conn = Connection::from_backend(backend);
    let Ok((globals, queue)) = globals::registry_queue_init::<Noop>(&conn) else {
        return false;
    };
    let Ok(activation) = globals.bind::<XdgActivationV1, _, _>(&queue.handle(), 1..=1, ()) else {
        return false;
    };
    // SAFETY: the pointer is winit's live wl_surface proxy for this window.
    let Ok(id) = (unsafe {
        wayland_backend::client::ObjectId::from_ptr(
            WlSurface::interface(),
            wh.surface.as_ptr().cast(),
        )
    }) else {
        return false;
    };
    let Ok(surface) = WlSurface::from_id(&conn, id) else {
        return false;
    };
    activation.activate(token.to_string(), &surface);
    activation.destroy();
    conn.flush().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_launch_hands_its_token_and_a_stale_socket_is_reclaimed() {
        let dir = std::env::temp_dir().join(format!("agentdeck-single-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gui.sock");
        let primary = claim_at(&path, "")
            .unwrap()
            .expect("first launch is the instance");
        assert!(claim_at(&path, "tok-1").unwrap().is_none());
        let (mut s, _) = primary.accept().unwrap();
        let mut got = String::new();
        s.read_to_string(&mut got).unwrap();
        assert_eq!(got, "show tok-1\n");
        // Crashed instance: the socket file stays but nobody listens.
        drop(primary);
        assert!(claim_at(&path, "").unwrap().is_some());
        std::fs::remove_dir_all(&dir).ok();
    }
}
