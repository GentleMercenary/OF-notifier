#![cfg_attr(	    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

#![feature(result_option_inspect)]
#![feature(let_chains)]

mod client;
mod structs;
mod settings;
mod deserializers;
mod websocket_client;
use cached::once_cell::sync::OnceCell;

#[macro_use]
extern crate log;
extern crate simplelog;

use tokio::select;
use chrono::Local;
use tempdir::TempDir;
use settings::Settings;
use futures::TryFutureExt;
use tokio_util::sync::CancellationToken;
use trayicon::{Icon, MenuBuilder, TrayIconBuilder};
use std::{
	fs::{self, File},
	path::Path,
	sync::Arc,
};
use winit::{
	event::Event,
	event_loop::{ControlFlow, EventLoop, EventLoopProxy},
};
use winrt_toast::{register, Toast, ToastManager, ToastDuration};
use simplelog::{Config, LevelFilter, WriteLogger, TermLogger, CombinedLogger, ColorChoice};

use crate::client::{OFClient, UnauthedClient, AuthedClient};

static MANAGER: OnceCell<ToastManager> = OnceCell::new();
static SETTINGS: OnceCell<Settings> = OnceCell::new();
static TEMPDIR: OnceCell<TempDir> = OnceCell::new();

fn register_app() -> anyhow::Result<()> {
	let aum_id = "OFNotifier";
	let icon_path = Path::new("res").join("icon.ico").canonicalize()?; // Doesn't work for some reason
	register(aum_id, "OF noitifier", Some(icon_path.as_path()))?;
	MANAGER
		.set(ToastManager::new(aum_id))
		.expect("Global toast manager set");

	TEMPDIR
		.set(TempDir::new("OF_thumbs")?)
		.expect("Temporary thumbnail created succesfully");
	Ok(())
}

async fn make_connection(proxy: EventLoopProxy<Events>, cancel_token: Arc<CancellationToken>) {
	let auth_link: &str = "https://onlyfans.com/api2/v2/users/me";
	info!("Fetching authentication parameters");

	let cloned_proxy = proxy.clone();
	OFClient::new().authorize()
	.and_then(|client| async move {
		let response = client.fetch(auth_link).await?;
		Ok((client, response))
	}).and_then(|(client, response)| async move {
		let text = response.text().await?;
		Ok((client, text))
	}).and_then(|(client, response)| async move {
		info!("Successful fetch for authentication parameters");
		
		let init_msg: structs::InitMessage = serde_json::from_str(&response)?;
		debug!("{:?}", init_msg);
		info!("Connecting as {}", init_msg.name);
		let mut socket = websocket_client::WebSocketClient::new()
			.connect(init_msg.ws_auth_token).await?;

		cloned_proxy.send_event(Events::Connected)?;
		let res = select! {
			_ = cancel_token.cancelled() => Ok(()),
			res = socket.message_loop(&client) => res,
		};

		info!("Terminating websocket");
		socket.close().await?;
		res
	})
	.unwrap_or_else(|err| {
		error!("Unexpected termination: {:?}", err);

		let mut toast = Toast::new();
		toast
			.text1("OF Notifier")
			.text2("An error occurred, disconnecting")
			.duration(ToastDuration::Long);

		MANAGER.wait().show(&toast).unwrap();
	})
	.await;

	proxy.send_event(Events::Disconnected).unwrap()
}

#[derive(PartialEq, Eq)]
enum State {
	Disconnected,
	Connecting,
	Connected,
}

#[derive(Clone, Eq, PartialEq, Debug)]
enum Events {
	ClickTrayIcon,
	Connected,
	Disconnected,
	Clear,
	Quit,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let log_folder = Path::new("logs");
	fs::create_dir_all(log_folder).expect("Created log directory");
	let mut log_path = log_folder.join(Local::now().format("%Y%m%d_%H%M%S").to_string());
	log_path.set_extension("log");

	CombinedLogger::init(
		vec![
			WriteLogger::new(if cfg!(debug_assertions) { LevelFilter::Debug } else { LevelFilter::Info }, Config::default(), File::create(log_path).expect("Created log file")),
			TermLogger::new(if cfg!(debug_assertions) { LevelFilter::Debug } else { LevelFilter::Info }, Config::default(), simplelog::TerminalMode::Mixed, ColorChoice::Auto)
		]
	)?;

	register_app().inspect_err(|err| error!("Error registering app: {}", err))?;

	let s = fs::read_to_string("settings.json")
		.inspect_err(|err| error!("Error reading settings.json: {}", err))?;

	SETTINGS
		.set(
			serde_json::from_str::<Settings>(&s)
				.inspect_err(|err| error!("Error parsing settings: {}", err))?,
		)
		.expect("Settings read properly");

	let event_loop = EventLoop::<Events>::with_user_event();
	let proxy = event_loop.create_proxy();
	let icon = include_bytes!("../res/icon.ico");
	let icon2 = include_bytes!("../res/icon2.ico");

	let first_icon = Icon::from_buffer(icon, None, None)?;
	let second_icon = Icon::from_buffer(icon2, None, None)?;

	let mut tray_icon = TrayIconBuilder::new()
		.sender_winit(proxy.clone())
		.icon_from_buffer(icon2)
		.tooltip("OF notifier")
		.on_click(Events::ClickTrayIcon)
		.menu(MenuBuilder::new()
		.item("Clear notifications", Events::Clear)
			.item("Quit", Events::Quit))
		.build()?;

	let mut state = State::Connecting;
	let mut cancel_token = Arc::new(CancellationToken::new());

	tokio::spawn(make_connection(proxy.clone(), cancel_token.clone()));
	event_loop.run(move |event, _, control_flow| {
		*control_flow = ControlFlow::Wait;
		let _ = tray_icon;

		if let Event::UserEvent(e) = event {
			match e {
				Events::ClickTrayIcon => {
					info!("Tray icon clicked");
					if state == State::Connected {
						info!("Disconnecting");
						cancel_token.cancel();
					} else if state == State::Disconnected {
						cancel_token = Arc::new(CancellationToken::new());
						info!("Connecting");
						state = State::Connecting;
						tokio::spawn(make_connection(proxy.clone(), cancel_token.clone()));
					}
				}
				Events::Connected => {
					tray_icon.set_icon(&first_icon).unwrap();
					state = State::Connected;
					info!("Connected");
				}
				Events::Disconnected => {
					tray_icon.set_icon(&second_icon).unwrap();
					state = State::Disconnected;
					info!("Disconnected");
				}
				Events::Quit => {
					info!("Closing application");
					cancel_token.cancel();
					MANAGER.wait().clear().unwrap();
					*control_flow = ControlFlow::Exit;
				},
				Events::Clear => {
					MANAGER.wait().clear().unwrap();
				}
			}
		}
	});
}

#[cfg(test)]
mod tests {
	use std::sync::Once;
	use std::thread::sleep;
	use std::time::Duration;

	use crate::settings::Whitelist;
	use simplelog::{ColorChoice, TermLogger, TerminalMode};
	use super::*;

	static INIT: Once = Once::new();

	fn init() {
		INIT.call_once(|| {
			register_app().unwrap();
			SETTINGS
				.set(Settings {
					notify: Whitelist::Full(true),
					download: Whitelist::Full(false),
				})
				.unwrap();
	
			TermLogger::init(
				LevelFilter::Debug,
				Config::default(),
				TerminalMode::Mixed,
				ColorChoice::Auto,
			)
			.unwrap();
		});
	}

	#[tokio::test]
	async fn test_chat_message() {
		init();

		let incoming = r#"{
			"api2_chat_message": {
				"id": 0,
				"text": "This is a message<br />\n to test <a href = \"/onlyfans\">MARKDOWN parsing</a> 👌<br />\n in notifications 💯",
				"price": 3.99,
				"fromUser": {
					"avatar": "https://public.onlyfans.com/files/m/mk/mka/mkamcrf6rjmcwo0jj4zoavhmalzohe5a1640180203/avatar.jpg",
					"id": 15585607,
					"name": "OnlyFans",
					"username": "onlyfans"
				},
				"media": [
					{
						"id": 0,
						"canView": true,
						"src": "https://raw.githubusercontent.com/allenbenz/winrt-notification/main/resources/test/chick.jpeg",
						"preview": "https://raw.githubusercontent.com/allenbenz/winrt-notification/main/resources/test/flower.jpeg",
						"type": "photo"
					}
				]
			}
		}"#;

		let msg = serde_json::from_str::<structs::MessageType>(incoming).unwrap();
		assert!(matches!(
			msg,
			structs::MessageType::Tagged(structs::TaggedMessageType::Api2ChatMessage(_))
		));

		let client = OFClient::new().authorize().await.unwrap();
		msg.handle_message(&client).await.unwrap();
		sleep(Duration::from_millis(1000));
	}

	#[tokio::test]
	async fn test_post_message() {
		init();

		// Onlyfan april fools post
		let incoming = r#"{
			"post_published": {
				"id": "129720708",
				"user_id" : "15585607",
				"show_posts_in_feed":true
			}
		}"#;

		let msg = serde_json::from_str::<structs::MessageType>(incoming).unwrap();
		assert!(matches!(
			msg,
			structs::MessageType::Tagged(structs::TaggedMessageType::PostPublished(_))
		));

		let client = OFClient::new().authorize().await.unwrap();
		msg.handle_message(&client).await.unwrap();
		sleep(Duration::from_millis(1000));
	}

	#[tokio::test]
	async fn test_story_message() {
		init();

		let incoming = r#"{
			"stories": [
				{
					"id": 0,
					"userId": 15585607,
					"media":[
						{
							"id": 0,
							"canView": true,
							"files": {
								"source": {
									"url": "https://raw.githubusercontent.com/allenbenz/winrt-notification/main/resources/test/chick.jpeg"
								},
								"preview": {
									"url": "https://raw.githubusercontent.com/allenbenz/winrt-notification/main/resources/test/flower.jpeg"
								}
							},
							"type": "photo"
						}
					]
				}
			]
		}"#;

		let msg = serde_json::from_str::<structs::MessageType>(incoming).unwrap();
		assert!(matches!(
			msg,
			structs::MessageType::Tagged(structs::TaggedMessageType::Stories(_))
		));

		let client = OFClient::new().authorize().await.unwrap();
		msg.handle_message(&client).await.unwrap();
		sleep(Duration::from_millis(1000));
	}

	
	#[tokio::test]
	async fn test_notification_message() {
		init();

		let incoming = r#"{
			"new_message":{
			   "id":"0",
			   "type":"message",
			   "text":"is currently running a promotion, <a href=\"https://onlyfans.com/onlyfans\">check it out</a>",
			   "subType":"promoreg_for_expired",
			   "user_id":"274000171",
			   "isRead":false,
			   "canGoToProfile":true,
			   "newPrice":null,
			   "user":{
					"avatar": "https://public.onlyfans.com/files/m/mk/mka/mkamcrf6rjmcwo0jj4zoavhmalzohe5a1640180203/avatar.jpg",
					"id": 15585607,
					"name": "OnlyFans",
					"username": "onlyfans"
				}
			},
			"hasSystemNotifications": false
		 }"#;

		let msg = serde_json::from_str::<structs::MessageType>(incoming).unwrap();
		assert!(matches!(
			msg,
			structs::MessageType::NewMessage(_)
		));

		let client = OFClient::new().authorize().await.unwrap();
		msg.handle_message(&client).await.unwrap();
		sleep(Duration::from_millis(1000));
	}

	#[tokio::test]
	async fn test_stream_message() {
		init();

		let incoming = r#"{
			"stream": {
				"id": 2611175,
				"description": "stream description",
				"title": "stream title",
				"startedAt": "2022-11-05T14:02:24+00:00",
				"room": "dc2-room-7dYNFuya8oYBRs1",
				"thumbUrl": "https://stream1-dc2.onlyfans.com/img/dc2-room-7dYNFuya8oYBRs1/thumb.jpg",
				"user": {
					"avatar": "https://public.onlyfans.com/files/m/mk/mka/mkamcrf6rjmcwo0jj4zoavhmalzohe5a1640180203/avatar.jpg",
					"id": 15585607,
					"name": "OnlyFans",
					"username": "onlyfans"
				}
			}
		}"#;

		let msg = serde_json::from_str::<structs::MessageType>(incoming).unwrap();
		assert!(matches!(
			msg,
			structs::MessageType::Tagged(structs::TaggedMessageType::Stream(_))
		));

		let client = OFClient::new().authorize().await.unwrap();
		msg.handle_message(&client).await.unwrap();
		sleep(Duration::from_millis(1000));
	}
}
