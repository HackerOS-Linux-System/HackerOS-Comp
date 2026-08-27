use std::{
    collections::HashMap,
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::Mutex,
};

use zbus::{
    fdo,
    interface,
    object_server::SignalContext,
    zvariant::{ObjectPath, OwnedFd, OwnedObjectPath, OwnedValue},
    Connection, ObjectServer,
};

/// Per-session bookkeeping — currently just the device-type bitmask
/// `SelectDevices` recorded, kept around for when a real consent UI
/// needs to show "this app wants keyboard+pointer access" or similar.
/// Bitmask values match the real portal spec's own convention:
/// 1 = keyboard, 2 = pointer, 4 = touchscreen.
#[derive(Default, Clone, Copy)]
struct Session {
    devices: u32,
}

struct PortalState {
    eis_socket_path: PathBuf,
    sessions: Mutex<HashMap<String, Session>>,
}

struct RemoteDesktopPortal {
    state: std::sync::Arc<PortalState>,
}

/// Exported dynamically, once per `handle` object path, by each of
/// `create_session`/`select_devices`/`start` — real
/// `org.freedesktop.impl.portal.Request` shape (an object a caller can
/// `Close()` to cancel a still-pending request, and which emits exactly
/// one `Response` signal when the request concludes, positively or
/// negatively). Stateless itself; the response value is provided by
/// whichever RemoteDesktopPortal method created it, immediately after
/// export, not stored on this struct.
struct RequestObject;

#[interface(name = "org.freedesktop.impl.portal.Request")]
impl RequestObject {
    /// Real requests would stop whatever's pending and emit a
    /// cancelled `Response` here — since every request in this
    /// implementation has already been auto-approved and resolved by
    /// the time a caller could possibly call `Close`, this is a no-op
    /// stub that exists for interface-shape completeness.
    async fn close(&self) {}

    #[zbus(signal)]
    async fn response(
        signal_ctxt: &SignalContext<'_>,
        response: u32,
        results: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;
}

/// Exports a `RequestObject` at `handle` and immediately emits a
/// success `Response(0, results)` signal on it — the "auto-approve"
/// simplification this module's doc is explicit about, factored out
/// since all three of `create_session`/`select_devices`/`start` do
/// exactly this same sequence.
async fn respond_success(
    object_server: &ObjectServer,
    handle: &ObjectPath<'_>,
    results: HashMap<String, OwnedValue>,
) -> fdo::Result<()> {
    let owned_handle = OwnedObjectPath::from(handle.to_owned());
    object_server
        .at(&owned_handle, RequestObject)
        .await
        .map_err(|e| fdo::Error::Failed(format!("failed to export Request object: {e}")))?;
    let iface_ref = object_server
        .interface::<_, RequestObject>(&owned_handle)
        .await
        .map_err(|e| fdo::Error::Failed(format!("failed to look up just-exported Request object: {e}")))?;
    RequestObject::response(iface_ref.signal_context(), 0, results)
        .await
        .map_err(|e| fdo::Error::Failed(format!("failed to emit Response signal: {e}")))?;
    Ok(())
}

#[interface(name = "org.freedesktop.impl.portal.RemoteDesktop")]
impl RemoteDesktopPortal {
    async fn create_session(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        _app_id: String,
        _options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] object_server: &ObjectServer,
    ) -> fdo::Result<()> {
        self.state
            .sessions
            .lock()
            .unwrap()
            .insert(session_handle.to_string(), Session::default());
        respond_success(object_server, &handle, HashMap::new()).await
    }

    async fn select_devices(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        _app_id: String,
        options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] object_server: &ObjectServer,
    ) -> fdo::Result<()> {
        // Real callers pass the requested device bitmask under the
        // "types" option key (u32) — this compositor doesn't currently
        // *restrict* what ConnectToEIS hands back based on it (the one
        // EIS socket serves pointer+keyboard+touch uniformly regardless
        // — see input_emulation::init/render/mod.rs's add_touch calls),
        // just records it for whenever a real consent UI wants to show
        // what was asked for.
        let devices = options
            .get("types")
            .and_then(|v| u32::try_from(v.clone()).ok())
            .unwrap_or(0);
        if let Some(session) = self.state.sessions.lock().unwrap().get_mut(&session_handle.to_string()) {
            session.devices = devices;
        }
        respond_success(object_server, &handle, HashMap::new()).await
    }

    async fn start(
        &self,
        handle: ObjectPath<'_>,
        _session_handle: ObjectPath<'_>,
        _app_id: String,
        _parent_window: String,
        _options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] object_server: &ObjectServer,
    ) -> fdo::Result<()> {
        respond_success(object_server, &handle, HashMap::new()).await
    }

    /// The one method with a genuinely immediate, synchronous result —
    /// real portal spec has this one return the fd directly rather than
    /// through the Request/Response pattern the other three use (there's
    /// nothing left to negotiate by this point; the session was already
    /// approved in `start`).
    async fn connect_to_eis(
        &self,
        _session_handle: ObjectPath<'_>,
        _app_id: String,
        _options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<OwnedFd> {
        let stream = UnixStream::connect(&self.state.eis_socket_path).map_err(|e| {
            fdo::Error::Failed(format!(
                "failed to connect to internal EIS socket at {}: {e}",
                self.state.eis_socket_path.display()
            ))
        })?;
        Ok(OwnedFd::from(std::os::fd::OwnedFd::from(stream)))
    }
}

pub fn init(eis_socket_path: PathBuf) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                tracing::warn!("portal: failed to start tokio runtime, RemoteDesktop portal disabled: {e}");
                return;
            }
        };
        rt.block_on(async move {
            let state = std::sync::Arc::new(PortalState {
                eis_socket_path,
                sessions: Mutex::new(HashMap::new()),
            });
            let portal = RemoteDesktopPortal { state };

            let connection: Connection = match Connection::session().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("portal: no D-Bus session bus available, RemoteDesktop portal disabled: {e}");
                    return;
                }
            };
            if let Err(e) = connection.object_server().at("/org/freedesktop/portal/desktop", portal).await {
                tracing::warn!("portal: failed to export RemoteDesktop interface: {e}");
                return;
            }
            if let Err(e) = connection.request_name("org.freedesktop.impl.portal.desktop.blue").await {
                tracing::warn!("portal: failed to claim D-Bus service name (another portal backend already running?): {e}");
                return;
            }

            tracing::info!(
                "RemoteDesktop D-Bus portal backend registered at \
                 org.freedesktop.impl.portal.desktop.blue — see portal/mod.rs's \
                 module doc for the real, load-bearing security caveat (no \
                 consent UI, auto-approves every request) before relying on \
                 this for anything real"
            );
            std::future::pending::<()>().await;
        });
    });
}
