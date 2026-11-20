//! Fields for a saved connection: driver, server, database and the sign-in values
//! the chosen method needs. A pasted connection string can fill them.

use gpui_kit::component::IndexPath;
use gpui_kit::component::button::Button;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, div, px,
};
use sql_db::{AuthMethod, Authentication, ConnectionSettings, DriverKind, Encryption};

use crate::connections::SavedConnection;

type LabelSelect = SelectState<Vec<SharedString>>;

pub struct ConnectionForm {
    driver: DriverKind,
    auth_method: AuthMethod,
    encryption: Encryption,
    trust_server_certificate: bool,
    /// No input field; copied from the loaded settings or a filled string.
    host_name_in_certificate: Option<String>,
    driver_select: Entity<LabelSelect>,
    auth_select: Entity<LabelSelect>,
    encryption_select: Entity<LabelSelect>,
    connection_string: Entity<InputState>,
    name: Entity<InputState>,
    server: Entity<InputState>,
    port: Entity<InputState>,
    database: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    tenant_id: Entity<InputState>,
    client_id: Entity<InputState>,
    client_secret: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl ConnectionForm {
    pub fn new(
        editing: Option<&SavedConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = editing.map_or_else(
            || ConnectionSettings::new(DriverKind::default()),
            |saved| saved.settings.clone(),
        );
        let driver_labels: Vec<SharedString> = DriverKind::ALL
            .iter()
            .map(|driver| SharedString::from(driver.label()))
            .collect();
        let driver_select = cx.new(|cx| SelectState::new(driver_labels, None, window, cx));
        let auth_select = cx.new(|cx| SelectState::new(Vec::new(), None, window, cx));
        let encryption_select = cx.new(|cx| SelectState::new(Vec::new(), None, window, cx));
        let text = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let secret = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .masked(true)
            })
        };
        let subscriptions = vec![
            cx.subscribe_in(&driver_select, window, Self::on_driver_selected),
            cx.subscribe_in(&auth_select, window, Self::on_auth_selected),
            cx.subscribe_in(&encryption_select, window, Self::on_encryption_selected),
        ];
        let mut form = Self {
            driver: settings.driver,
            auth_method: AuthMethod::from(&settings.authentication),
            encryption: settings.encryption,
            trust_server_certificate: settings.trust_server_certificate,
            host_name_in_certificate: None,
            driver_select,
            auth_select,
            encryption_select,
            connection_string: secret(
                "paste a connection string, then Fill from string",
                window,
                cx,
            ),
            name: text("BI warehouse", window, cx),
            server: text(r"host, or host\instance", window, cx),
            port: text("default", window, cx),
            database: text("database", window, cx),
            user: text("user name", window, cx),
            password: secret("password", window, cx),
            tenant_id: text("directory (tenant) ID", window, cx),
            client_id: text("application (client) ID", window, cx),
            client_secret: secret("client secret value", window, cx),
            _subscriptions: subscriptions,
        };
        if let Some(saved) = editing {
            set_text(&form.name, &saved.name, window, cx);
        }
        form.fill(&settings, window, cx);
        form
    }

    /// The name and settings, or a message saying which field is missing.
    pub fn saved_connection(&self, cx: &App) -> Result<SavedConnection, String> {
        let value = |input: &Entity<InputState>| input.read(cx).value().trim().to_owned();
        let required = |input: &Entity<InputState>, field: &str| {
            let text = value(input);
            if text.is_empty() {
                Err(format!("Enter the {field}."))
            } else {
                Ok(text)
            }
        };
        let name = required(&self.name, "connection name")?;
        let server = required(&self.server, "server")?;
        if !value(&self.connection_string).is_empty() {
            return Err("Press Fill from string, or clear the connection string box.".to_owned());
        }
        // Hidden fields keep their values, so a Fabric endpoint with an unusual
        // port or certificate name keeps working after an edit.
        let port = match value(&self.port) {
            text if text.is_empty() => None,
            text => Some(
                text.parse::<u16>()
                    .map_err(|_| format!("The port must be a number, not {text}."))?,
            ),
        };
        let authentication = match self.auth_method {
            AuthMethod::Password => Authentication::Password {
                user: required(&self.user, "user name")?,
                // Passwords may legitimately start or end with spaces.
                password: self.password.read(cx).value().to_string(),
            },
            AuthMethod::AzureCli => Authentication::AzureCli,
            AuthMethod::ServicePrincipal => Authentication::ServicePrincipal {
                tenant_id: required(&self.tenant_id, "tenant ID")?,
                client_id: required(&self.client_id, "client ID")?,
                client_secret: self.client_secret.read(cx).value().to_string(),
            },
        };
        if let Authentication::ServicePrincipal { client_secret, .. } = &authentication
            && client_secret.is_empty()
        {
            return Err("Enter the client secret.".to_owned());
        }
        let settings = ConnectionSettings {
            driver: self.driver,
            server,
            port,
            database: value(&self.database),
            authentication,
            encryption: self.encryption,
            trust_server_certificate: self.trust_server_certificate,
            host_name_in_certificate: self.host_name_in_certificate.clone(),
        };
        settings
            .config()
            .map_err(|error| format!("Server: {error}"))?;
        Ok(SavedConnection { name, settings })
    }

    fn fill(&mut self, settings: &ConnectionSettings, window: &mut Window, cx: &mut Context<Self>) {
        self.set_driver(settings.driver, window, cx);
        self.set_auth_method(AuthMethod::from(&settings.authentication), window, cx);
        self.set_encryption(settings.encryption, window, cx);
        self.trust_server_certificate = settings.trust_server_certificate;
        self.host_name_in_certificate
            .clone_from(&settings.host_name_in_certificate);
        set_text(&self.server, &settings.server, window, cx);
        let port = settings
            .port
            .map(|port| port.to_string())
            .unwrap_or_default();
        set_text(&self.port, &port, window, cx);
        set_text(&self.database, &settings.database, window, cx);
        match &settings.authentication {
            Authentication::Password { user, password } => {
                set_text(&self.user, user, window, cx);
                set_text(&self.password, password, window, cx);
            }
            Authentication::AzureCli => {}
            Authentication::ServicePrincipal {
                tenant_id,
                client_id,
                client_secret,
            } => {
                set_text(&self.tenant_id, tenant_id, window, cx);
                set_text(&self.client_id, client_id, window, cx);
                set_text(&self.client_secret, client_secret, window, cx);
            }
        }
        cx.notify();
    }

    fn fill_from_connection_string(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.connection_string.read(cx).value().trim().to_owned();
        if text.is_empty() {
            window.push_notification("Paste a connection string first.", cx);
            return;
        }
        match ConnectionSettings::from_connection_string(&text) {
            Ok(settings)
                if !settings
                    .driver
                    .auth_methods()
                    .contains(&AuthMethod::from(&settings.authentication)) =>
            {
                window.push_notification(
                    format!(
                        "{} does not accept {}.",
                        settings.driver.label(),
                        AuthMethod::from(&settings.authentication).label(settings.driver)
                    ),
                    cx,
                );
            }
            Ok(settings) => {
                self.fill(&settings, window, cx);
                // The fields now hold its values, and a password should not linger here.
                set_text(&self.connection_string, "", window, cx);
            }
            Err(error) => window.push_notification(format!("Connection string: {error}"), cx),
        }
    }

    fn set_driver(&mut self, driver: DriverKind, window: &mut Window, cx: &mut Context<Self>) {
        self.driver = driver;
        let index = DriverKind::ALL
            .iter()
            .position(|candidate| *candidate == driver);
        self.driver_select.update(cx, |select, cx| {
            select.set_selected_index(index.map(IndexPath::new), window, cx);
        });
        let labels: Vec<SharedString> = driver
            .auth_methods()
            .iter()
            .map(|method| SharedString::from(method.label(driver)))
            .collect();
        self.auth_select
            .update(cx, |select, cx| select.set_items(labels, window, cx));
        let encryption_labels: Vec<SharedString> = driver
            .encryption_choices()
            .iter()
            .map(|(_, label)| SharedString::from(*label))
            .collect();
        self.encryption_select.update(cx, |select, cx| {
            select.set_items(encryption_labels, window, cx);
        });
        let encryption = if driver
            .encryption_choices()
            .iter()
            .any(|(choice, _)| *choice == self.encryption)
        {
            self.encryption
        } else {
            driver
                .encryption_choices()
                .first()
                .map_or(self.encryption, |(choice, _)| *choice)
        };
        self.set_encryption(encryption, window, cx);
        let method = if driver.auth_methods().contains(&self.auth_method) {
            self.auth_method
        } else {
            driver
                .auth_methods()
                .first()
                .copied()
                .unwrap_or(AuthMethod::AzureCli)
        };
        self.set_auth_method(method, window, cx);
    }

    fn set_encryption(
        &mut self,
        encryption: Encryption,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.encryption = encryption;
        let index = self
            .driver
            .encryption_choices()
            .iter()
            .position(|(choice, _)| *choice == encryption);
        self.encryption_select.update(cx, |select, cx| {
            select.set_selected_index(index.map(IndexPath::new), window, cx);
        });
    }

    fn set_auth_method(&mut self, method: AuthMethod, window: &mut Window, cx: &mut Context<Self>) {
        self.auth_method = method;
        let index = self
            .driver
            .auth_methods()
            .iter()
            .position(|candidate| *candidate == method);
        self.auth_select.update(cx, |select, cx| {
            select.set_selected_index(index.map(IndexPath::new), window, cx);
        });
        cx.notify();
    }

    fn on_driver_selected(
        &mut self,
        select: &Entity<LabelSelect>,
        _: &SelectEvent<Vec<SharedString>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = select.read(cx).selected_index(cx) else {
            return;
        };
        let Some(driver) = DriverKind::ALL.get(index.row).copied() else {
            return;
        };
        if driver != self.driver {
            // A port, encryption level or certificate choice made for another
            // driver means something else here, or is hidden.
            set_text(&self.port, "", window, cx);
            self.trust_server_certificate = false;
            self.host_name_in_certificate = None;
            self.encryption = ConnectionSettings::new(driver).encryption;
        }
        self.set_driver(driver, window, cx);
    }

    fn on_auth_selected(
        &mut self,
        select: &Entity<LabelSelect>,
        _: &SelectEvent<Vec<SharedString>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = select.read(cx).selected_index(cx) else {
            return;
        };
        if let Some(method) = self.driver.auth_methods().get(index.row) {
            self.auth_method = *method;
            cx.notify();
        }
    }

    fn on_encryption_selected(
        &mut self,
        select: &Entity<LabelSelect>,
        _: &SelectEvent<Vec<SharedString>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = select.read(cx).selected_index(cx) else {
            return;
        };
        if let Some((encryption, _)) = self.driver.encryption_choices().get(index.row) {
            self.encryption = *encryption;
            cx.notify();
        }
    }

    fn sign_in_rows(&self) -> Vec<AnyElement> {
        match self.auth_method {
            AuthMethod::Password => vec![
                row("User", Input::new(&self.user)),
                row("Password", Input::new(&self.password).mask_toggle()),
            ],
            AuthMethod::AzureCli => vec![row(
                "",
                div()
                    .text_xs()
                    .child("Uses the token from your `az login` session."),
            )],
            AuthMethod::ServicePrincipal => vec![
                row("Tenant ID", Input::new(&self.tenant_id)),
                row("Client ID", Input::new(&self.client_id)),
                row(
                    "Client secret",
                    Input::new(&self.client_secret).mask_toggle(),
                ),
            ],
        }
    }

    fn network_rows(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        if !self.driver.has_network_options() {
            return Vec::new();
        }
        let form = cx.weak_entity();
        vec![
            row("Encryption", Select::new(&self.encryption_select)),
            row(
                "",
                Checkbox::new("trust-server-certificate")
                    .label("Trust the server certificate (self-signed servers)")
                    .checked(self.trust_server_certificate)
                    .on_click(move |checked, _, cx| {
                        let checked = *checked;
                        if let Err(error) = form.update(cx, |form, cx| {
                            form.trust_server_certificate = checked;
                            cx.notify();
                        }) {
                            tracing::debug!(%error, "connection form closed");
                        }
                    }),
            ),
        ]
    }
}

impl Focusable for ConnectionForm {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl Render for ConnectionForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let server_row = if self.driver.has_network_options() {
            row(
                "Server",
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.server)))
                    .child(div().w(px(90.)).child(Input::new(&self.port))),
            )
        } else {
            row("Server", Input::new(&self.server))
        };
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.connection_string).mask_toggle()),
                    )
                    .child(
                        Button::new("fill-from-string")
                            .label("Fill from string")
                            .outline()
                            .on_click(cx.listener(|form, _, window, cx| {
                                form.fill_from_connection_string(window, cx);
                            })),
                    ),
            )
            .child(row("Name", Input::new(&self.name)))
            .child(row("Driver", Select::new(&self.driver_select)))
            .child(server_row)
            .child(row("Database", Input::new(&self.database)))
            .child(row("Sign-in", Select::new(&self.auth_select)))
            .children(self.sign_in_rows())
            .children(self.network_rows(cx))
    }
}

fn row(label: &'static str, field: impl IntoElement) -> AnyElement {
    h_flex()
        .gap_3()
        .child(div().w(px(110.)).flex_none().text_sm().child(label))
        .child(div().flex_1().min_w_0().child(field))
        .into_any_element()
}

fn set_text(
    input: &Entity<InputState>,
    text: &str,
    window: &mut Window,
    cx: &mut Context<ConnectionForm>,
) {
    let text = text.to_owned();
    input.update(cx, |input, cx| input.set_value(text, window, cx));
}
