//! Entra sign-in prompts as notifications, and refresh tokens in [`crate::secrets`].

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::{App, ClipboardItem, Context, Global, Task, Window};
use sql_db::entra::{self, SignInHooks, SignInPrompt};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::secrets;

/// The prompt receiver until the main window takes it.
struct Prompts(Option<UnboundedReceiver<SignInPrompt>>);

impl Global for Prompts {}

/// Identifies the sign-in notification, so it can be replaced and removed.
struct SignInNotice;

pub fn init(cx: &mut App) {
    let (sender, receiver) = unbounded_channel();
    entra::set_sign_in_hooks(SignInHooks {
        prompt: Box::new(move |prompt| {
            if sender.send(prompt).is_err() {
                tracing::debug!("no window shows sign-in prompts");
            }
        }),
        load_refresh_token: Box::new(secrets::get),
        save_refresh_token: Box::new(|key, token| {
            if let Err(error) = secrets::set(key, token) {
                tracing::warn!(%error, "could not store the Entra refresh token");
            }
        }),
    });
    cx.set_global(Prompts(Some(receiver)));
}

/// Shows prompts in `window` for as long as the returned task lives.
pub fn show_prompts<T: 'static>(window: &mut Window, cx: &mut Context<T>) -> Option<Task<()>> {
    let mut receiver = cx.global_mut::<Prompts>().0.take()?;
    Some(cx.spawn_in(window, async move |_, cx| {
        while let Some(prompt) = receiver.recv().await {
            if cx.update(|window, cx| show(prompt, window, cx)).is_err() {
                break;
            }
        }
    }))
}

fn show(prompt: SignInPrompt, window: &mut Window, cx: &mut App) {
    window.remove_notification::<SignInNotice>(cx);
    match prompt {
        SignInPrompt::OpenBrowser { url } => {
            cx.open_url(&url);
            let note = Notification::info(
                "Finish signing in in the browser window that opened. Click here to open it again.",
            )
            .title("Entra ID sign-in")
            .id::<SignInNotice>()
            .autohide(false)
            .on_click(move |_, _, cx| cx.open_url(&url));
            window.push_notification(note, cx);
        }
        SignInPrompt::DeviceCode {
            verification_uri,
            user_code,
        } => {
            cx.write_to_clipboard(ClipboardItem::new_string(user_code.clone()));
            let note = Notification::info(format!(
                "Enter the code {user_code} (copied to the clipboard) at {verification_uri}. Click here to open the page."
            ))
            .title("Entra ID sign-in")
            .id::<SignInNotice>()
            .autohide(false)
            .on_click(move |_, _, cx| {
                // The address comes from the sign-in answer; only a web page is opened.
                if verification_uri.starts_with("https://") {
                    cx.open_url(&verification_uri);
                }
            });
            window.push_notification(note, cx);
        }
        SignInPrompt::Finished => {}
    }
}
