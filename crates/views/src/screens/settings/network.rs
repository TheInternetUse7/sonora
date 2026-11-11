use gpui::prelude::*;
use gpui::{AnyElement, App, Context, Entity, SharedString, div};
use i18n::t;
use net::{Policy, Settings};
use ui::{Button, Input, MenuItem, Picker};

use super::{Row, SettingsView};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Inherit,
    Automatic,
    Direct,
    Custom,
}

pub(super) struct Form {
    scope: String,
    mode: Mode,
    pub address: Entity<Input>,
    pub bypass: Entity<Input>,
    invalid: bool,
    revealed: bool,
}

impl Mode {
    fn key(self) -> &'static str {
        match self {
            Self::Inherit => "settings-proxy-inherit",
            Self::Automatic => "settings-proxy-automatic",
            Self::Direct => "settings-proxy-direct",
            Self::Custom => "settings-proxy-custom",
        }
    }

    fn of(policy: Option<&Policy>) -> Self {
        match policy {
            None => Self::Inherit,
            Some(Policy::Automatic) => Self::Automatic,
            Some(Policy::Direct) => Self::Direct,
            Some(Policy::Custom { .. }) => Self::Custom,
        }
    }
}

impl Form {
    pub fn new(settings: &Settings, cx: &mut Context<SettingsView>) -> Self {
        let address = cx.new(|cx| {
            Input::new("settings-proxy-address-hint", cx)
                .name("settings-proxy-address")
                .compact()
                .masked()
        });
        let bypass = cx.new(|cx| {
            Input::new("settings-proxy-bypass-hint", cx)
                .name("settings-proxy-bypass")
                .compact()
        });
        let mut form = Self {
            scope: String::new(),
            mode: Mode::Automatic,
            address,
            bypass,
            invalid: false,
            revealed: false,
        };
        form.load(settings, cx);
        form
    }

    fn load(&mut self, settings: &Settings, cx: &mut Context<SettingsView>) {
        let policy = match self.scope.is_empty() {
            true => Some(&settings.global),
            false => settings.providers.get(&self.scope),
        };
        self.mode = Mode::of(policy);
        let (url, bypass) = match policy {
            Some(Policy::Custom { url, bypass }) => (url.as_str(), bypass.as_str()),
            _ => ("", ""),
        };
        self.address.update(cx, |input, cx| {
            input.set_text(url, cx);
            input.set_invalid(false, cx);
            input.set_masked(true, cx);
        });
        self.bypass
            .update(cx, |input, cx| input.set_text(bypass, cx));
        self.invalid = false;
        self.revealed = false;
    }

    fn policy(&self, cx: &App) -> Option<Policy> {
        match self.mode {
            Mode::Inherit => None,
            Mode::Automatic => Some(Policy::Automatic),
            Mode::Direct => Some(Policy::Direct),
            Mode::Custom => Some(Policy::Custom {
                url: self.address.read(cx).text().trim().to_owned(),
                bypass: self.bypass.read(cx).text().trim().to_owned(),
            }),
        }
    }
}

impl SettingsView {
    pub(super) fn network_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let scopes: Vec<_> = std::iter::once((String::new(), t!("settings-proxy-global")))
            .chain(
                self.session
                    .read(cx)
                    .providers()
                    .map(|provider| (provider.slug.to_owned(), SharedString::from(provider.name))),
            )
            .collect();
        let current = scopes
            .iter()
            .find(|(slug, _)| *slug == self.network.scope)
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| t!("settings-proxy-global"));
        let scope = Picker::new("proxy-scope", &self.popovers, current)
            .name(t!("settings-proxy-scope"))
            .width(Picker::REGULAR)
            .items(scopes.into_iter().map(|(slug, label)| {
                MenuItem::new(SharedString::from(format!("proxy-scope-{slug}")), label)
                    .selected(slug == self.network.scope)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.network.scope = slug.clone();
                        let settings = this.settings.read(cx).network().clone();
                        this.network.load(&settings, cx);
                        this.remeasure(cx);
                    }))
            }));
        let mode = Picker::new(
            "proxy-mode",
            &self.popovers,
            i18n::lookup(self.network.mode.key(), None),
        )
        .name(t!("settings-proxy-mode"))
        .width(Picker::REGULAR)
        .items(
            [Mode::Inherit, Mode::Automatic, Mode::Direct, Mode::Custom]
                .into_iter()
                .filter(|mode| *mode != Mode::Inherit || !self.network.scope.is_empty())
                .map(|mode| {
                    MenuItem::new(mode.key(), i18n::lookup(mode.key(), None))
                        .selected(self.network.mode == mode)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.network.mode = mode;
                            this.network.invalid = false;
                            this.network
                                .address
                                .update(cx, |input, cx| input.set_invalid(false, cx));
                            this.remeasure(cx);
                        }))
                }),
        );
        let detail = match self.network.mode {
            Mode::Inherit => {
                let mode = i18n::lookup(
                    Mode::of(Some(&self.settings.read(cx).network().global)).key(),
                    None,
                );
                t!("settings-proxy-inherited", mode = mode.as_ref())
            }
            Mode::Automatic => t!("settings-proxy-automatic-detail"),
            Mode::Direct => t!("settings-proxy-direct-detail"),
            Mode::Custom => t!("settings-proxy-custom-detail"),
        };
        div()
            .flex()
            .flex_col()
            .w_full()
            .gap_3()
            .child(Row::new(
                t!("settings-proxy-scope"),
                scope.into_any_element(),
            ))
            .child(Row::new(t!("settings-proxy-mode"), mode.into_any_element()).detail(detail))
            .when(self.network.mode == Mode::Custom, |column| {
                column
                    .child(Row::new(
                        t!("settings-proxy-address"),
                        Button::new("proxy-reveal")
                            .small()
                            .ghost()
                            .label(match self.network.revealed {
                                true => t!("settings-proxy-hide"),
                                false => t!("settings-proxy-show"),
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.network.revealed = !this.network.revealed;
                                this.network.address.update(cx, |input, cx| {
                                    input.set_masked(!this.network.revealed, cx)
                                });
                                cx.notify();
                            }))
                            .into_any_element(),
                    ))
                    .child(self.network.address.clone())
                    .child(
                        Row::new(t!("settings-proxy-bypass"), div().into_any_element())
                            .detail(t!("settings-proxy-bypass-detail")),
                    )
                    .child(self.network.bypass.clone())
            })
            .child(
                Row::new(
                    t!("settings-network"),
                    Button::new("proxy-save")
                        .label(t!("settings-proxy-save"))
                        .on_click(cx.listener(|this, _, _, cx| this.save_network(cx)))
                        .into_any_element(),
                )
                .detail(
                    match (
                        self.network.invalid,
                        self.settings.read(cx).network_pending(),
                    ) {
                        (true, _) => t!("settings-proxy-invalid"),
                        (false, true) => t!("settings-proxy-restart"),
                        _ => t!("settings-proxy-save-detail"),
                    },
                ),
            )
            .into_any_element()
    }

    fn save_network(&mut self, cx: &mut Context<Self>) {
        let mut settings = self.settings.read(cx).network().clone();
        let policy = self.network.policy(cx);
        match (self.network.scope.is_empty(), policy) {
            (true, Some(policy)) => settings.global = policy,
            (false, Some(policy)) => {
                settings
                    .providers
                    .insert(self.network.scope.clone(), policy);
            }
            (false, None) => {
                settings.providers.remove(&self.network.scope);
            }
            (true, None) => return,
        }
        self.network.invalid = self
            .settings
            .update(cx, |state, cx| state.set_network(settings, cx))
            .is_err();
        self.network
            .address
            .update(cx, |input, cx| input.set_invalid(self.network.invalid, cx));
        self.remeasure(cx);
    }
}
