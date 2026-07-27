//! Custom `org.freedesktop.DBus.Properties` interface implementation.
//!
//! The default zbus `Properties` implementation rejects an empty interface name
//! when calling `GetAll`, but systemd accepts `GetAll("")` to return all
//! properties across all interfaces on the object.  We replace the default
//! implementation with this one, which parses the interface-name argument as a
//! plain `String` (no D-Bus name validation) and behaves the same as systemd
//! for the empty-string case.

use std::collections::HashMap;
use std::fmt::Write;

use async_trait::async_trait;
use libsysa::l10n;
use zbus::object_server::{DispatchResult, Interface, SignalContext};
use zbus::names::InterfaceName;
use zbus::{Connection, ObjectServer, fdo};
use zvariant::OwnedValue;

use once_cell::sync::OnceCell;
use std::sync::Arc;

use crate::state::AllocatorHandle;
use crate::unit::types::UnitKind;
use super::manager::ManagerInterface;
use super::service_obj::ServiceObject;
use super::socket_obj::SocketObject;
use super::unit_obj::UnitObject;

/// Replacement for `zbus::fdo::Properties` that accepts an empty interface
/// name in `GetAll`, mirroring systemd's behaviour.
pub struct Properties {
    pub allocator: AllocatorHandle,
    pub unit_name: String,
}

impl Properties {
    /// Collect all properties exposed by a specific interface on this unit.
    ///
    /// Returns `None` for unknown interface names.
    async fn props_for_interface(
        &self,
        iface_name: &str,
    ) -> Option<fdo::Result<HashMap<String, OwnedValue>>> {
        match iface_name {
            "org.freedesktop.systemd1.Unit" => {
                let obj = UnitObject {
                    allocator: self.allocator.clone(),
                    unit_name: self.unit_name.clone(),
                };
                Some(obj.get_all().await)
            }
            "org.freedesktop.systemd1.Service" => {
                let kind = self
                    .allocator
                    .read()
                    .units
                    .get(&self.unit_name)
                    .map(|u| u.kind.clone());
                if matches!(kind, Some(UnitKind::Service)) {
                    let obj = ServiceObject {
                        allocator: self.allocator.clone(),
                        unit_name: self.unit_name.clone(),
                    };
                    Some(obj.get_all().await)
                } else {
                    None
                }
            }
            "org.freedesktop.systemd1.Socket" => {
                let kind = self
                    .allocator
                    .read()
                    .units
                    .get(&self.unit_name)
                    .map(|u| u.kind.clone());
                if matches!(kind, Some(UnitKind::Socket)) {
                    let obj = SocketObject {
                        allocator: self.allocator.clone(),
                        unit_name: self.unit_name.clone(),
                    };
                    Some(obj.get_all().await)
                } else {
                    None
                }
            }
            "org.freedesktop.systemd1.Slice" => {
                let kind = self
                    .allocator
                    .read()
                    .units
                    .get(&self.unit_name)
                    .map(|u| u.kind.clone());
                if matches!(kind, Some(UnitKind::Slice)) {
                    // SliceObject has no properties, but return an empty map rather than None.
                    Some(Ok(HashMap::new()))
                } else {
                    None
                }
            }
            // The standard built-in interfaces carry no user-visible properties.
            "org.freedesktop.DBus.Properties"
            | "org.freedesktop.DBus.Peer"
            | "org.freedesktop.DBus.Introspectable"
            | "org.freedesktop.DBus.ObjectManager" => Some(Ok(HashMap::new())),
            _ => None,
        }
    }

    /// Collect all properties from every interface registered on this object.
    async fn all_props(&self) -> HashMap<String, OwnedValue> {
        let mut result = HashMap::new();

        // Unit interface is always present.
        let unit_obj = UnitObject {
            allocator: self.allocator.clone(),
            unit_name: self.unit_name.clone(),
        };
        if let Ok(props) = unit_obj.get_all().await {
            result.extend(props);
        }

        // Type-specific interface, if applicable.
        let kind = self
            .allocator
            .read()
            .units
            .get(&self.unit_name)
            .map(|u| u.kind.clone());

        match kind {
            Some(UnitKind::Service) => {
                let obj = ServiceObject {
                    allocator: self.allocator.clone(),
                    unit_name: self.unit_name.clone(),
                };
                if let Ok(props) = obj.get_all().await {
                    result.extend(props);
                }
            }
            Some(UnitKind::Socket) => {
                let obj = SocketObject {
                    allocator: self.allocator.clone(),
                    unit_name: self.unit_name.clone(),
                };
                if let Ok(props) = obj.get_all().await {
                    result.extend(props);
                }
            }
            _ => {}
        }

        result
    }

    async fn handle_get_all(
        &self,
        connection: &Connection,
        msg: &zbus::message::Message,
    ) -> zbus::Result<()> {
        let body = msg.body();
        let (iface_name,): (String,) = match body.deserialize() {
            Ok(r) => r,
            Err(e) => {
                let err = fdo::Error::InvalidArgs(l10n::fmt(l10n::t_("Bad arguments: {e}."), &[("e", &e.to_string())]));
                connection.reply_dbus_error(&msg.header(), err).await?;
                return Ok(());
            }
        };

        if iface_name.is_empty() {
            // systemd extension: empty interface name → return all properties.
            let props = self.all_props().await;
            connection.reply(msg, &props).await?;
        } else {
            match self.props_for_interface(&iface_name).await {
                Some(Ok(props)) => {
                    connection.reply(msg, &props).await?;
                }
                Some(Err(e)) => {
                    connection.reply_dbus_error(&msg.header(), e).await?;
                }
                None => {
                    let err = fdo::Error::UnknownInterface(l10n::fmt(
                        l10n::t_("Unknown interface '{iface_name}'."),
                        &[("iface_name", &iface_name)],
                    ));
                    connection.reply_dbus_error(&msg.header(), err).await?;
                }
            }
        }
        Ok(())
    }

    async fn handle_get(
        &self,
        connection: &Connection,
        msg: &zbus::message::Message,
    ) -> zbus::Result<()> {
        let body = msg.body();
        let (iface_name, prop_name): (String, String) = match body.deserialize() {
            Ok(r) => r,
            Err(e) => {
                let err = fdo::Error::InvalidArgs(l10n::fmt(l10n::t_("Bad arguments: {e}."), &[("e", &e.to_string())]));
                connection.reply_dbus_error(&msg.header(), err).await?;
                return Ok(());
            }
        };

        // First check that the interface is known — return UnknownInterface
        // before checking individual properties, per D-Bus spec.
        let is_known_interface = matches!(
            iface_name.as_str(),
            "org.freedesktop.systemd1.Unit"
                | "org.freedesktop.systemd1.Service"
                | "org.freedesktop.systemd1.Socket"
                | "org.freedesktop.systemd1.Slice"
                | "org.freedesktop.DBus.Properties"
                | "org.freedesktop.DBus.Peer"
                | "org.freedesktop.DBus.Introspectable"
                | "org.freedesktop.DBus.ObjectManager"
        );
        if !is_known_interface {
            let err = fdo::Error::UnknownInterface(l10n::fmt(
                l10n::t_("Unknown interface '{iface_name}'."),
                &[("iface_name", &iface_name)],
            ));
            connection.reply_dbus_error(&msg.header(), err).await?;
            return Ok(());
        }

        let value = match iface_name.as_str() {
            "org.freedesktop.systemd1.Unit" => {
                let obj = UnitObject {
                    allocator: self.allocator.clone(),
                    unit_name: self.unit_name.clone(),
                };
                obj.get(&prop_name).await
            }
            "org.freedesktop.systemd1.Service" => {
                let obj = ServiceObject {
                    allocator: self.allocator.clone(),
                    unit_name: self.unit_name.clone(),
                };
                obj.get(&prop_name).await
            }
            "org.freedesktop.systemd1.Socket" => {
                let obj = SocketObject {
                    allocator: self.allocator.clone(),
                    unit_name: self.unit_name.clone(),
                };
                obj.get(&prop_name).await
            }
            "org.freedesktop.systemd1.Slice" => None,
            // Standard built-in interfaces have no properties.
            "org.freedesktop.DBus.Properties"
            | "org.freedesktop.DBus.Peer"
            | "org.freedesktop.DBus.Introspectable"
            | "org.freedesktop.DBus.ObjectManager" => None,
            _ => unreachable!(), // checked above
        };

        match value {
            Some(Ok(v)) => {
                // GetAll returns a{sv}, but Get returns a bare Variant (v).
                // Wrap the OwnedValue in a zvariant::Value for the reply.
                let v: zvariant::Value<'_> = v.into();
                connection.reply(msg, &v).await?;
            }
            Some(Err(e)) => {
                connection.reply_dbus_error(&msg.header(), e).await?;
            }
            None => {
                let err = fdo::Error::UnknownProperty(l10n::fmt(
                    l10n::t_("Unknown property '{prop_name}'."),
                    &[("prop_name", &prop_name)],
                ));
                connection.reply_dbus_error(&msg.header(), err).await?;
            }
        }
        Ok(())
    }

    async fn handle_set(
        &self,
        connection: &Connection,
        msg: &zbus::message::Message,
    ) -> zbus::Result<()> {
        // All properties exposed by systema are read-only.
        let err = fdo::Error::PropertyReadOnly("Properties are read-only".to_string());
        connection.reply_dbus_error(&msg.header(), err).await?;
        Ok(())
    }
}

#[async_trait]
impl Interface for Properties {
    fn name() -> InterfaceName<'static>
    where
        Self: Sized,
    {
        InterfaceName::from_static_str_unchecked("org.freedesktop.DBus.Properties")
    }

    async fn get(&self, _property_name: &str) -> Option<fdo::Result<OwnedValue>> {
        // The Properties meta-interface itself has no properties.
        None
    }

    async fn get_all(&self) -> fdo::Result<HashMap<String, OwnedValue>> {
        Ok(HashMap::new())
    }

    async fn set_mut(
        &mut self,
        _property_name: &str,
        _value: &zvariant::Value<'_>,
        _ctxt: &SignalContext<'_>,
    ) -> Option<fdo::Result<()>> {
        None
    }

    fn call<'call>(
        &'call self,
        _server: &'call ObjectServer,
        connection: &'call Connection,
        msg: &'call zbus::message::Message,
        name: zbus::names::MemberName<'call>,
    ) -> DispatchResult<'call> {
        match name.as_str() {
            "GetAll" => DispatchResult::Async(Box::pin(async move {
                self.handle_get_all(connection, msg).await.map_err(Into::into)
            })),
            "Get" => DispatchResult::Async(Box::pin(async move {
                self.handle_get(connection, msg).await.map_err(Into::into)
            })),
            "Set" => DispatchResult::Async(Box::pin(async move {
                self.handle_set(connection, msg).await.map_err(Into::into)
            })),
            _ => DispatchResult::NotFound,
        }
    }

    fn call_mut<'call>(
        &'call mut self,
        _server: &'call ObjectServer,
        _connection: &'call Connection,
        _msg: &'call zbus::message::Message,
        _name: zbus::names::MemberName<'call>,
    ) -> DispatchResult<'call> {
        DispatchResult::NotFound
    }

    fn introspect_to_writer(&self, writer: &mut dyn Write, level: usize) {
        writeln!(
            writer,
            "{:indent$}<interface name=\"org.freedesktop.DBus.Properties\">",
            "",
            indent = level
        )
        .unwrap();
        let l = level + 2;
        // Get
        writeln!(writer, "{:indent$}<method name=\"Get\">", "", indent = l).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"s\" name=\"interface_name\" direction=\"in\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"s\" name=\"property_name\" direction=\"in\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"v\" name=\"value\" direction=\"out\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}</method>", "", indent = l).unwrap();
        // GetAll
        writeln!(writer, "{:indent$}<method name=\"GetAll\">", "", indent = l).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"s\" name=\"interface_name\" direction=\"in\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"a{{sv}}\" name=\"properties\" direction=\"out\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}</method>", "", indent = l).unwrap();
        // Set
        writeln!(writer, "{:indent$}<method name=\"Set\">", "", indent = l).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"s\" name=\"interface_name\" direction=\"in\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"s\" name=\"property_name\" direction=\"in\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"v\" name=\"value\" direction=\"in\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}</method>", "", indent = l).unwrap();
        // PropertiesChanged signal
        writeln!(writer, "{:indent$}<signal name=\"PropertiesChanged\">", "", indent = l).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"s\" name=\"interface_name\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"a{{sv}}\" name=\"changed_properties\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"as\" name=\"invalidated_properties\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}</signal>", "", indent = l).unwrap();
        writeln!(writer, "{:indent$}</interface>", "", indent = level).unwrap();
    }
}

// ---------------------------------------------------------------------------
// ManagerProperties — custom org.freedesktop.DBus.Properties for the
// manager object (/org/freedesktop/systemd1).
//
// Same motivation as `Properties` above: zbus's default implementation
// rejects an empty interface name, but systemd accepts `GetAll("")` on the
// manager path and returns all Manager properties.
// ---------------------------------------------------------------------------

/// Custom `org.freedesktop.DBus.Properties` for the manager object.
pub struct ManagerProperties {
    pub allocator: AllocatorHandle,
}

impl ManagerProperties {
    /// Collect all properties exposed by `org.freedesktop.systemd1.Manager`.
    async fn manager_iface_props(&self) -> fdo::Result<HashMap<String, OwnedValue>> {
        let obj = ManagerInterface::new(
            self.allocator.clone(),
            Arc::new(OnceCell::new()),
        );
        obj.get_all().await
    }

    async fn handle_get_all(
        &self,
        connection: &Connection,
        msg: &zbus::message::Message,
    ) -> zbus::Result<()> {
        let body = msg.body();
        let (iface_name,): (String,) = match body.deserialize() {
            Ok(r) => r,
            Err(e) => {
                let err = fdo::Error::InvalidArgs(l10n::fmt(l10n::t_("Bad arguments: {e}."), &[("e", &e.to_string())]));
                connection.reply_dbus_error(&msg.header(), err).await?;
                return Ok(());
            }
        };

        match iface_name.as_str() {
            // systemd extension: empty interface name → return all Manager properties.
            "" | "org.freedesktop.systemd1.Manager" => {
                match self.manager_iface_props().await {
                    Ok(props) => {
                        connection.reply(msg, &props).await?;
                    }
                    Err(e) => {
                        connection.reply_dbus_error(&msg.header(), e).await?;
                    }
                }
            }
            // Standard built-in interfaces carry no user-visible properties.
            "org.freedesktop.DBus.Properties"
            | "org.freedesktop.DBus.Peer"
            | "org.freedesktop.DBus.Introspectable"
            | "org.freedesktop.DBus.ObjectManager" => {
                let empty: HashMap<String, OwnedValue> = HashMap::new();
                connection.reply(msg, &empty).await?;
            }
            _ => {
                let err = fdo::Error::UnknownInterface(l10n::fmt(
                    l10n::t_("Unknown interface '{iface_name}'."),
                    &[("iface_name", &iface_name)],
                ));
                connection.reply_dbus_error(&msg.header(), err).await?;
            }
        }
        Ok(())
    }

    async fn handle_get(
        &self,
        connection: &Connection,
        msg: &zbus::message::Message,
    ) -> zbus::Result<()> {
        let body = msg.body();
        let (iface_name, prop_name): (String, String) = match body.deserialize() {
            Ok(r) => r,
            Err(e) => {
                let err = fdo::Error::InvalidArgs(l10n::fmt(l10n::t_("Bad arguments: {e}."), &[("e", &e.to_string())]));
                connection.reply_dbus_error(&msg.header(), err).await?;
                return Ok(());
            }
        };

        if iface_name != "org.freedesktop.systemd1.Manager" {
            let err = fdo::Error::UnknownInterface(l10n::fmt(
                l10n::t_("Unknown interface '{iface_name}'."),
                &[("iface_name", &iface_name)],
            ));
            connection.reply_dbus_error(&msg.header(), err).await?;
            return Ok(());
        }

        let obj = ManagerInterface::new(
            self.allocator.clone(),
            Arc::new(OnceCell::new()),
        );
        match obj.get(&prop_name).await {
            Some(Ok(v)) => {
                let v: zvariant::Value<'_> = v.into();
                connection.reply(msg, &v).await?;
            }
            Some(Err(e)) => {
                connection.reply_dbus_error(&msg.header(), e).await?;
            }
            None => {
                let err = fdo::Error::UnknownProperty(l10n::fmt(
                    l10n::t_("Unknown property '{prop_name}'."),
                    &[("prop_name", &prop_name)],
                ));
                connection.reply_dbus_error(&msg.header(), err).await?;
            }
        }
        Ok(())
    }

    async fn handle_set(
        &self,
        connection: &Connection,
        msg: &zbus::message::Message,
    ) -> zbus::Result<()> {
        let err = fdo::Error::PropertyReadOnly("Properties are read-only".to_string());
        connection.reply_dbus_error(&msg.header(), err).await?;
        Ok(())
    }
}

#[async_trait]
impl Interface for ManagerProperties {
    fn name() -> InterfaceName<'static>
    where
        Self: Sized,
    {
        InterfaceName::from_static_str_unchecked("org.freedesktop.DBus.Properties")
    }

    async fn get(&self, _property_name: &str) -> Option<fdo::Result<OwnedValue>> {
        None
    }

    async fn get_all(&self) -> fdo::Result<HashMap<String, OwnedValue>> {
        Ok(HashMap::new())
    }

    async fn set_mut(
        &mut self,
        _property_name: &str,
        _value: &zvariant::Value<'_>,
        _ctxt: &SignalContext<'_>,
    ) -> Option<fdo::Result<()>> {
        None
    }

    fn call<'call>(
        &'call self,
        _server: &'call ObjectServer,
        connection: &'call Connection,
        msg: &'call zbus::message::Message,
        name: zbus::names::MemberName<'call>,
    ) -> DispatchResult<'call> {
        match name.as_str() {
            "GetAll" => DispatchResult::Async(Box::pin(async move {
                self.handle_get_all(connection, msg)
                    .await
                    .map_err(Into::into)
            })),
            "Get" => DispatchResult::Async(Box::pin(async move {
                self.handle_get(connection, msg).await.map_err(Into::into)
            })),
            "Set" => DispatchResult::Async(Box::pin(async move {
                self.handle_set(connection, msg).await.map_err(Into::into)
            })),
            _ => DispatchResult::NotFound,
        }
    }

    fn call_mut<'call>(
        &'call mut self,
        _server: &'call ObjectServer,
        _connection: &'call Connection,
        _msg: &'call zbus::message::Message,
        _name: zbus::names::MemberName<'call>,
    ) -> DispatchResult<'call> {
        DispatchResult::NotFound
    }

    fn introspect_to_writer(&self, writer: &mut dyn Write, level: usize) {
        writeln!(
            writer,
            "{:indent$}<interface name=\"org.freedesktop.DBus.Properties\">",
            "",
            indent = level
        )
        .unwrap();
        let l = level + 2;
        writeln!(writer, "{:indent$}<method name=\"Get\">", "", indent = l).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"s\" name=\"interface_name\" direction=\"in\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"s\" name=\"property_name\" direction=\"in\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"v\" name=\"value\" direction=\"out\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}</method>", "", indent = l).unwrap();
        writeln!(writer, "{:indent$}<method name=\"GetAll\">", "", indent = l).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"s\" name=\"interface_name\" direction=\"in\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"a{{sv}}\" name=\"properties\" direction=\"out\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}</method>", "", indent = l).unwrap();
        writeln!(writer, "{:indent$}<method name=\"Set\">", "", indent = l).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"s\" name=\"interface_name\" direction=\"in\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"s\" name=\"property_name\" direction=\"in\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"v\" name=\"value\" direction=\"in\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}</method>", "", indent = l).unwrap();
        writeln!(writer, "{:indent$}<signal name=\"PropertiesChanged\">", "", indent = l).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"s\" name=\"interface_name\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"a{{sv}}\" name=\"changed_properties\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}<arg type=\"as\" name=\"invalidated_properties\"/>", "", indent = l + 2).unwrap();
        writeln!(writer, "{:indent$}</signal>", "", indent = l).unwrap();
        writeln!(writer, "{:indent$}</interface>", "", indent = level).unwrap();
    }
}
