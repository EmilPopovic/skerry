// SPDX-License-Identifier: Apache-2.0
//! Mounting a disk, which needs somebody with more privilege than this process.
//!
//! **[ADR 0030](../../../docs/adr/0030-a-volume-is-a-device-udev-described-and-mounting-it-is-one-udisks2-call.md).**
//! [`sk_fs::volumes`] finds a USB stick nothing has mounted; this is how it gets
//! mounted. `mount(2)` needs root and [rule 9](../../../docs/03-principles.md)
//! forbids `GVfs`, which leaves udisks2 — one system service, one method call, and
//! polkit deciding whether this person may.
//!
//! # Three calls and no subscription
//!
//! Each of the three below is a *method* call, so
//! [ADR 0023](../../../docs/adr/0023-d-bus-is-a-socket-we-speak-ourselves.md)'s
//! short-lived connection per call is exactly the right shape and its revisit
//! trigger — a feature that needs to receive signals — is not fired. Hearing
//! about a disk being plugged in does not come from here: it is a
//! `NETLINK_KOBJECT_UEVENT` socket in [`sk_fs::watch`].
//!
//! # The object path is asked for, never assembled
//!
//! udisks2 names a block device `/org/freedesktop/UDisks2/block_devices/sdb1`,
//! and the last component is the device's name with everything outside
//! `[A-Za-z0-9]` escaped by a rule that is udisks2's rather than a
//! specification's. `Manager.ResolveDevice` answers the same question for the
//! cost of one round trip, and a round trip is cheaper than a rule that is
//! right on this machine's device names and wrong on somebody's.

use std::path::{Path, PathBuf};

use crate::bus::wire::Value;
use crate::bus::{Bus, Message};

/// Why a disk was not mounted.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refused {
    /// There is no system bus to ask, which is a session with no D-Bus at all.
    NoBus,
    /// The bus is there and udisks2 is not — a machine that mounts disks some
    /// other way, or does not mount them.
    NoService,
    /// polkit said no. The action id, so the message can say which permission.
    NotAllowed(String),
    /// udisks2 tried and could not: a filesystem it has no driver for, a disk
    /// that was pulled out between the press and the call.
    Failed(String),
}

impl Refused {
    /// The catalogue key for what to tell a person.
    #[must_use]
    pub const fn label_key(&self) -> &'static str {
        match self {
            Self::NoBus => "error.no_system_bus",
            Self::NoService => "error.no_udisks",
            Self::NotAllowed(_) => "error.mount_not_allowed",
            Self::Failed(_) => "error.mount_failed",
        }
    }

    /// The detail for the log, which is the chain rather than the sentence.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::NoBus | Self::NoService => "",
            Self::NotAllowed(it) | Self::Failed(it) => it,
        }
    }
}

/// The service, and the three interfaces used here.
const SERVICE: &str = "org.freedesktop.UDisks2";
const MANAGER: &str = "/org/freedesktop/UDisks2/Manager";
const MANAGER_INTERFACE: &str = "org.freedesktop.UDisks2.Manager";
const FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";
const BLOCK: &str = "org.freedesktop.UDisks2.Block";
const DRIVE: &str = "org.freedesktop.UDisks2.Drive";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// Whether this machine has a udisks2 to ask.
///
/// **Probed, never compiled in** — [ADR
/// 0007](../../../docs/adr/0007-runtime-capability-detection.md). One connection
/// and one round trip, on a job, so a row can be drawn dimmed with a reason
/// rather than drawn and then failing when it is pressed.
#[must_use]
pub fn available() -> bool {
    // **Its version, not a device.** The first version of this asked
    // `ResolveDevice` about `/` and read the refusal as "no udisks2" — and `/`
    // is a mount point rather than a device, so the answer was
    // `Invalid device specification provided` from a service that was running
    // and answering. A property every version has is a question that cannot be
    // wrong about its subject.
    let Some(mut bus) = Bus::system() else { return false };
    let answer = bus.call_for(Message::call(
        SERVICE,
        MANAGER,
        PROPERTIES,
        "Get",
        vec![Value::Str(MANAGER_INTERFACE.to_owned()), Value::Str("Version".to_owned())],
    ));
    answer.is_ok()
}

/// Mounts `device`. Where udisks2 put it, or why it did not.
///
/// **Runs on a job**: a connection, an authentication and two round trips, and
/// the mount itself is a syscall in another process that may take as long as the
/// filesystem does.
///
/// # Errors
///
/// [`Refused`], which carries what to say and what to log separately.
pub fn mount(device: &Path) -> Result<PathBuf, Refused> {
    let mut bus = Bus::system().ok_or(Refused::NoBus)?;
    let object = resolve(&mut bus, device)?;
    let answer = bus
        .call_for(Message::call(
            SERVICE,
            &object,
            FILESYSTEM,
            "Mount",
            vec![Value::Dict(Vec::new())],
        ))
        .map_err(refusal)?;
    // **The mount point comes back from the call.** Reading the mount table
    // instead would be a second question with a race in it: the answer is
    // authoritative and it is already here.
    answer.text().map(PathBuf::from).ok_or_else(|| Refused::Failed("no mount point".to_owned()))
}

/// Unmounts `device`.
///
/// **Runs on a job.**
///
/// # Errors
///
/// [`Refused`].
pub fn unmount(device: &Path) -> Result<(), Refused> {
    let mut bus = Bus::system().ok_or(Refused::NoBus)?;
    let object = resolve(&mut bus, device)?;
    bus.call_for(Message::call(
        SERVICE,
        &object,
        FILESYSTEM,
        "Unmount",
        vec![Value::Dict(Vec::new())],
    ))
    .map_err(refusal)?;
    Ok(())
}

/// Ejects the drive `device` sits on, after unmounting it.
///
/// **Unmount first, and its failure is the whole failure.** A drive with a
/// mounted filesystem on it does not eject, and udisks2's error for that names
/// the busy filesystem rather than the thing a person did — so the unmount is
/// done here and reported as itself.
///
/// **Not `PowerOff`**, which is what cuts a USB stick's power and is a different
/// decision: ADR 0030 names it as deliberately absent. A drive with no eject
/// mechanism — every memory stick — refuses, and the refusal is reported rather
/// than swallowed, because "safely remove" that silently did nothing is worse
/// than a sentence.
///
/// **Runs on a job.**
///
/// # Errors
///
/// [`Refused`].
pub fn eject(device: &Path) -> Result<(), Refused> {
    let mut bus = Bus::system().ok_or(Refused::NoBus)?;
    let object = resolve(&mut bus, device)?;
    // A filesystem that is not mounted refuses the unmount, which is not a
    // reason to stop: the eject is what was asked for.
    let _ = bus.call_for(Message::call(
        SERVICE,
        &object,
        FILESYSTEM,
        "Unmount",
        vec![Value::Dict(Vec::new())],
    ));
    let drive = property(&mut bus, &object, BLOCK, "Drive")?;
    if drive == "/" {
        return Err(Refused::Failed("the device is on no drive".to_owned()));
    }
    bus.call_for(Message::call(SERVICE, &drive, DRIVE, "Eject", vec![Value::Dict(Vec::new())]))
        .map_err(refusal)?;
    Ok(())
}

/// The object path udisks2 knows `device` by, for a probe that mounts nothing.
///
/// **Public because a resolve is the only harmless half of this module.** A
/// scenario and a bring-up probe both need to know that the service answers
/// about a real device, and neither should have to mount somebody's disk to
/// find out.
///
/// # Errors
///
/// [`Refused`].
pub fn resolved(device: &Path) -> Result<String, Refused> {
    let mut bus = Bus::system().ok_or(Refused::NoBus)?;
    resolve(&mut bus, device)
}

/// The object path udisks2 knows `device` by.
fn resolve(bus: &mut Bus, device: &Path) -> Result<String, Refused> {
    // **The bare string, because `Dict` wraps its own values.** Wrapping it here
    // too sent udisks2 a variant holding a variant holding a string, and its
    // answer was `Invalid device specification provided` — which reads as a bad
    // device path and was a bad *encoding*. See [`Value::Dict`].
    let spec =
        Value::Dict(vec![("path".to_owned(), Value::Str(device.to_string_lossy().into_owned()))]);
    let answer = bus
        .call_for(Message::call(
            SERVICE,
            MANAGER,
            MANAGER_INTERFACE,
            "ResolveDevice",
            vec![spec, Value::Dict(Vec::new())],
        ))
        .map_err(refusal)?;
    // `ao`, and a device udisks2 does not know is an empty one rather than an
    // error — which is the case a disk pulled out between the press and the call
    // produces.
    let found = match answer.body.first() {
        Some(Value::Array(paths)) => paths.iter().find_map(path_of),
        _ => None,
    };
    found.ok_or_else(|| Refused::Failed(format!("udisks2 does not know {}", device.display())))
}

/// One property, as an object path.
fn property(bus: &mut Bus, object: &str, interface: &str, name: &str) -> Result<String, Refused> {
    let answer = bus
        .call_for(Message::call(
            SERVICE,
            object,
            PROPERTIES,
            "Get",
            vec![Value::Str(interface.to_owned()), Value::Str(name.to_owned())],
        ))
        .map_err(refusal)?;
    answer
        .body
        .first()
        .and_then(path_of)
        .ok_or_else(|| Refused::Failed(format!("{interface}.{name} is not an object path")))
}

/// An object path out of a value, through however many variants wrap it.
fn path_of(value: &Value) -> Option<String> {
    match value {
        Value::Path(it) => Some(it.clone()),
        Value::Variant(inner) => path_of(inner),
        _ => None,
    }
}

/// Which kind of refusal a D-Bus error is.
///
/// **The name decides, not the text.** polkit's refusal and a filesystem driver
/// that is missing read the same to a person who is shown the raw error, and
/// they are two different things to fix: one is a permission and the other is a
/// package.
fn refusal(error: String) -> Refused {
    if error.starts_with("org.freedesktop.DBus.Error.ServiceUnknown")
        || error.starts_with("org.freedesktop.DBus.Error.NameHasNoOwner")
    {
        return Refused::NoService;
    }
    if error.contains("NotAuthorized") || error.contains("org.freedesktop.PolicyKit") {
        return Refused::NotAllowed(error);
    }
    Refused::Failed(error)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{Refused, path_of, refusal};
    use crate::bus::wire::Value;

    /// **The name decides, and each kind has its own sentence.** A permission
    /// and a missing driver read identically as a raw error and are two
    /// different things to do about it.
    #[test]
    fn a_refusal_is_sorted_by_the_errors_own_name() {
        assert_eq!(
            refusal("org.freedesktop.DBus.Error.ServiceUnknown: no such name".to_owned()),
            Refused::NoService
        );
        assert!(matches!(
            refusal("org.freedesktop.UDisks2.Error.NotAuthorized: not permitted".to_owned()),
            Refused::NotAllowed(_)
        ));
        assert!(matches!(
            refusal("org.freedesktop.UDisks2.Error.Failed: unknown filesystem".to_owned()),
            Refused::Failed(_)
        ));
    }

    /// Every kind says something, and the sentence is separate from the chain —
    /// rule 8's own line between a message and a log.
    #[test]
    fn every_refusal_has_a_sentence_and_keeps_its_detail_out_of_it() {
        for refused in [
            Refused::NoBus,
            Refused::NoService,
            Refused::NotAllowed("polkit said no".to_owned()),
            Refused::Failed("no driver".to_owned()),
        ] {
            let key = refused.label_key();
            assert!(key.starts_with("error."), "{key}");
            assert_ne!(
                sk_model::i18n::lookup(key),
                key,
                "{key} has no catalogue entry — its name resolved to its own key"
            );
            assert!(!refused.detail().contains("error."), "the detail is not the sentence");
        }
        assert_eq!(Refused::NoBus.detail(), "", "there is nothing to log about an absent bus");
    }

    /// udisks2 answers `ao` and `Properties.Get` answers a variant around an
    /// `o`, so the reader has to see through one.
    #[test]
    fn an_object_path_is_read_through_a_variant() {
        let bare = Value::Path("/org/freedesktop/UDisks2/block_devices/sdb1".to_owned());
        assert_eq!(path_of(&bare).as_deref(), Some("/org/freedesktop/UDisks2/block_devices/sdb1"));
        let wrapped = Value::Variant(Box::new(bare));
        assert!(path_of(&wrapped).is_some());
        assert_eq!(path_of(&Value::Str("/not/a/path".to_owned())), None);
    }
}
