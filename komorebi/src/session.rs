//! Persistencia de sesión: guarda el mapa ventana→workspace para poder
//! restaurarlo tras un `rset` (reinicio de komorebi con las apps vivas).
//!
//! Solo es fiable mientras las apps no se cierren: los window id de la
//! Accessibility API son estables durante la vida del proceso, pero cambian
//! tras reiniciar el Mac o cerrar/abrir la app. Por eso esto cubre el caso
//! del `rset`, no el del reinicio del sistema.

use crate::DATA_DIR;
use crate::window_manager::WindowManager;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct SessionState {
    /// UUID del arranque del sistema (kern.bootsessionuuid). Solo restauramos
    /// la sesión si coincide con el arranque actual: tras un reinicio del Mac
    /// los window id se reasignan y podrían colisionar por azar con ids viejos,
    /// colocando ventanas en workspaces equivocados. Cambiar de arranque ⇒
    /// descartar la sesión (las ventanas caen en su workspace por defecto).
    #[serde(default)]
    pub boot_uuid: String,
    pub windows: Vec<SessionWindow>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionWindow {
    pub window_id: u32,
    /// Nombre de la app (komorebi exe). Permite emparejar por app+título
    /// cuando el window id ya no sirve (tras logout/login las apps reabren
    /// con ids nuevos pero el mismo arranque del sistema).
    #[serde(default)]
    pub exe: String,
    #[serde(default)]
    pub title: String,
    pub monitor: usize,
    pub workspace: usize,
}

impl SessionState {
    /// Busca el (monitor, workspace) recordado para una ventana y consume la
    /// entrada (para que dos ventanas no reclamen la misma). Prioridad:
    ///   1. window id + app  → exacto, caso rset (apps vivas).
    ///   2. app + título     → best-effort, caso logout/login (ids nuevos).
    /// Exigir que la app coincida evita colocar mal una ventana si un id nuevo
    /// colisiona por azar con uno viejo de otra app.
    pub fn take_match(&mut self, window_id: u32, exe: &str, title: &str) -> Option<(usize, usize)> {
        if let Some(pos) = self
            .windows
            .iter()
            .position(|w| w.window_id == window_id && w.exe == exe)
        {
            let w = self.windows.remove(pos);
            return Some((w.monitor, w.workspace));
        }

        if !title.is_empty()
            && let Some(pos) = self
                .windows
                .iter()
                .position(|w| w.exe == exe && w.title == title)
        {
            let w = self.windows.remove(pos);
            return Some((w.monitor, w.workspace));
        }

        None
    }
}

fn session_path() -> PathBuf {
    DATA_DIR.join("komorebi.session.json")
}

/// UUID del arranque actual del sistema, vía `sysctl -n kern.bootsessionuuid`.
/// Estable durante todo el arranque (sobrevive a un `rset`) y distinto tras
/// reiniciar. Sin dependencias extra. Se calcula una sola vez (no cambia
/// durante la vida del proceso) porque save() se invoca en cada evento.
fn boot_uuid() -> Option<String> {
    static CACHED: OnceLock<Option<String>> = OnceLock::new();

    CACHED
        .get_or_init(|| {
            let output = std::process::Command::new("sysctl")
                .args(["-n", "kern.bootsessionuuid"])
                .output()
                .ok()?;

            if !output.status.success() {
                return None;
            }

            let uuid = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if uuid.is_empty() { None } else { Some(uuid) }
        })
        .clone()
}

// Cache de lo último escrito para no reescribir el archivo si nada cambió.
static LAST_WRITTEN: Mutex<Option<String>> = Mutex::new(None);

/// Lee el estado de sesión guardado, si lo hay y es del arranque actual.
pub fn load() -> Option<SessionState> {
    let contents = std::fs::read_to_string(session_path()).ok()?;
    let state: SessionState = serde_json::from_str(&contents).ok()?;

    // Solo válida dentro del mismo arranque (caso rset). Tras reiniciar el
    // Mac, descartamos la sesión para no colocar ventanas por ids colisionados.
    match boot_uuid() {
        Some(current) if current == state.boot_uuid => Some(state),
        _ => {
            tracing::info!("ignoring session from a previous boot");
            None
        }
    }
}

/// Construye el estado actual y lo guarda en disco (solo si cambió).
pub fn save(wm: &WindowManager) {
    let state = build(wm);

    let json = match serde_json::to_string_pretty(&state) {
        Ok(json) => json,
        Err(error) => {
            tracing::warn!("could not serialize session state: {error}");
            return;
        }
    };

    let mut last = match LAST_WRITTEN.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    if last.as_deref() == Some(json.as_str()) {
        return;
    }

    if let Err(error) = std::fs::write(session_path(), &json) {
        tracing::warn!("could not write session state: {error}");
        return;
    }

    *last = Some(json);
}

fn build(wm: &WindowManager) -> SessionState {
    let mut windows = Vec::new();
    let boot_uuid = boot_uuid().unwrap_or_default();

    for (m_idx, monitor) in wm.monitors.elements().iter().enumerate() {
        for (w_idx, workspace) in monitor.workspaces().iter().enumerate() {
            for container in workspace.containers() {
                for window in container.windows() {
                    windows.push(SessionWindow {
                        window_id: window.id,
                        exe: window.exe().unwrap_or_default(),
                        title: window.title().unwrap_or_default(),
                        monitor: m_idx,
                        workspace: w_idx,
                    });
                }
            }

            for window in workspace.floating_windows() {
                windows.push(SessionWindow {
                    window_id: window.id,
                    exe: window.exe().unwrap_or_default(),
                    title: window.title().unwrap_or_default(),
                    monitor: m_idx,
                    workspace: w_idx,
                });
            }
        }
    }

    SessionState { boot_uuid, windows }
}
