use qemu_display::{Console, Display, MouseButton};

use ironrdp_server::{
    KeyboardEvent, MouseButton as RdpMouseButton, MouseEvent, RdpServerInputHandler,
};
use tokio::{
    sync::mpsc::{Receiver, Sender},
    task,
};

pub struct InputHandler {
    tx: Sender<InputEvent>,
    _task: task::JoinHandle<()>,
}

#[derive(Debug)]
enum InputEvent {
    Keyboard(KeyboardEvent),
    Mouse(MouseEvent),
}

impl RdpServerInputHandler for InputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        tracing::debug!(?event);
        if let Err(e) = self.tx.try_send(InputEvent::Keyboard(event)) {
            eprintln!("keyboard error: {e:?}");
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        tracing::trace!(?event);
        if let Err(e) = self.tx.try_send(InputEvent::Mouse(event)) {
            eprintln!("mouse error: {e:?}");
        }
    }
}

async fn input_receive_task(mut rx: Receiver<InputEvent>, console: Console) {
    loop {
        let res = match rx.recv().await {
            Some(InputEvent::Keyboard(ev)) => match ev {
                KeyboardEvent::Pressed { code, .. } => console.keyboard.press(code as u32).await,
                KeyboardEvent::Released { code, .. } => console.keyboard.release(code as u32).await,
                KeyboardEvent::Synchronize(flags) => {
                    tracing::debug!(?flags, "lock keys sync not supported yet");
                    // console.keyboard.set_modifiers(0).await
                    Ok(())
                }
                other => {
                    eprintln!("unhandled keyboard event: {other:?}");
                    Ok(())
                }
            },
            Some(InputEvent::Mouse(ev)) => match ev {
                MouseEvent::Move { x, y } => {
                    tracing::trace!(?x, ?y);
                    console.mouse.set_abs_position(x.into(), y.into()).await
                }
                MouseEvent::Button {
                    button, pressed, ..
                } => {
                    let button = match button {
                        RdpMouseButton::Left => Some(MouseButton::Left),
                        RdpMouseButton::Right => Some(MouseButton::Right),
                        RdpMouseButton::Middle => Some(MouseButton::Middle),
                        RdpMouseButton::X1 => Some(MouseButton::Side),
                        RdpMouseButton::X2 => Some(MouseButton::Extra),
                        // `MouseButton` is non-exhaustive upstream.
                        other => {
                            eprintln!("unhandled mouse button: {other:?}");
                            None
                        }
                    };
                    match button {
                        Some(button) if pressed => console.mouse.press(button).await,
                        Some(button) => console.mouse.release(button).await,
                        None => Ok(()),
                    }
                }
                MouseEvent::VerticalScroll { value } => {
                    let motion = if value > 0 {
                        MouseButton::WheelUp
                    } else {
                        MouseButton::WheelDown
                    };

                    console.mouse.press(motion).await
                }
                other => {
                    eprintln!("unhandled input event: {other:?}");
                    Ok(())
                }
            },
            None => break,
        };

        if let Err(e) = res {
            eprintln!("input handling error: {e:?}");
        }
    }
}

impl InputHandler {
    pub async fn connect(display: &Display<'_>) -> anyhow::Result<InputHandler> {
        let console = Console::new(display.connection(), 0).await?;
        let (tx, rx) = tokio::sync::mpsc::channel(30);
        let _task = task::spawn(async move { input_receive_task(rx, console).await });

        Ok(Self { _task, tx })
    }
}
