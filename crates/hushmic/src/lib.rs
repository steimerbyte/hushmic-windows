pub mod about;
// The Linux desktop stack: PipeWire graph configuration, the StatusNotifierItem
// tray, D-Bus (notifications, portal, KGlobalAccel), systemd units and Unix
// sockets. Windows replaces each of these through `win::*`.
pub mod abtest;
#[cfg(unix)]
pub mod autostart;
pub mod branding;
pub mod config;
pub mod config_cli;
pub mod control;
#[cfg(unix)]
pub mod controller;
#[cfg(unix)]
pub mod diagnostics;
pub mod fsutil;
pub mod i18n;
#[cfg(unix)]
pub mod lock;
#[cfg(unix)]
pub mod mictest;
#[cfg(unix)]
pub mod notify;
#[cfg(unix)]
pub mod pipewire;
#[cfg(unix)]
pub mod portal;
#[cfg(unix)]
pub mod sandbox;
#[cfg(unix)]
pub mod service;
#[cfg(unix)]
pub mod shortcuts;
#[cfg(unix)]
pub mod tray;
#[cfg(unix)]
pub mod traylink;
#[cfg(unix)]
pub mod watchdog;
pub mod win;
