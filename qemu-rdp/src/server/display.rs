use std::{
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    thread,
};

use anyhow::Result;
use ironrdp_server::{DisplayContext, ServerError, ServerErrorExt, ServerResult};
use qemu_display::{
    Console, ConsoleListenerHandler, ConsoleListenerMapHandler, Cursor, Display, MouseSet, Scanout,
    ScanoutMap, ScanoutMmap, Update, UpdateMap,
};

use ironrdp::{
    connector::DesktopSize,
    displaycontrol::pdu::DisplayControlMonitorLayout,
    server::{BitmapUpdate, DisplayUpdate, RGBAPointer, RdpServerDisplay, RdpServerDisplayUpdates},
};
use tracing::{debug, warn};

use crate::{cast, utils::PixmanFormat};

mod queue;
use queue::DisplayQueue;

pub struct DisplayHandler {
    console: Console,
}

struct DisplayUpdates {
    receiver: queue::Receiver,
    console: Console,
}

impl Drop for DisplayUpdates {
    fn drop(&mut self) {
        self.console.unregister_listener();
    }
}

impl DisplayHandler {
    pub async fn connect(display: &Display<'_>) -> Result<Self> {
        let console = Console::new(display.connection(), 0).await?;

        Ok(Self { console })
    }

    async fn listen(&self) -> Result<DisplayUpdates> {
        let (sender, receiver) = DisplayQueue::channel();
        let (width, height) = (
            self.console.width().await? as _,
            self.console.height().await? as _,
        );
        let desktop_size = DesktopSize { width, height };
        let listener = Listener::new(sender, desktop_size);
        self.console.unregister_listener();
        self.console.register_listener(listener.clone()).await?;
        #[cfg(any(windows, unix))]
        self.console.set_map_listener(listener.clone()).await?;

        let console = self.console.clone();
        Ok(DisplayUpdates { receiver, console })
    }
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for DisplayUpdates {
    async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
        Ok(self.receiver.recv().await)
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for DisplayHandler {
    async fn size(&mut self) -> DesktopSize {
        let width = self.console.proxy.width().await.unwrap() as u16;
        let height = self.console.proxy.height().await.unwrap() as u16;
        DesktopSize { height, width }
    }

    async fn updates(&mut self, _ctx: DisplayContext) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        let updates = self
            .listen()
            .await
            .map_err(|e| ServerError::reason("qemu console listener", format!("{e:#}")))?;

        Ok(Box::new(updates))
    }

    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        let console = self.console.proxy.clone();
        // TODO: use a queue and a dedicated task/thread for requests, to preserve order
        thread::spawn(move || {
            // TODO: multi-monitor
            let Some(monitor) = layout.monitors().first() else {
                return;
            };
            let (xoff, yoff) = monitor.position().unwrap_or_default();
            let (width, height) = monitor.dimensions();
            let (width_mm, height_mm) = monitor.physical_dimensions().unwrap_or_default();
            crate::utils::block_on(async move {
                let _ = console
                    .set_ui_info(width_mm as u16, height_mm as u16, xoff, yoff, width, height)
                    .await;
            });
        });
    }
}

struct Inner {
    sender: queue::Sender,
    desktop_size: DesktopSize,
    cursor_hot: (i32, i32),
    scanout_mmap: Option<ScanoutMmap>,
}

#[derive(Clone)]
struct Listener {
    inner: Arc<Mutex<Inner>>,
}

impl Listener {
    fn new(sender: queue::Sender, desktop_size: DesktopSize) -> Self {
        let inner = Inner {
            sender,
            desktop_size,
            cursor_hot: (0, 0),
            scanout_mmap: None,
        };
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    async fn send(&mut self, update: DisplayUpdate) {
        let sender = self.inner.lock().unwrap().sender.clone();

        if let Err(e) = sender.send(update).await {
            println!("{e:?}");
        };
    }

    async fn set_desktop_size(&mut self, width: u32, height: u32) {
        let desktop_size = DesktopSize {
            width: cast!(width),
            height: cast!(height),
        };

        debug!(?desktop_size);

        {
            let mut inner = self.inner.lock().unwrap();

            if desktop_size == inner.desktop_size {
                return;
            }

            inner.desktop_size = desktop_size;
        }

        self.send(DisplayUpdate::Resize(desktop_size)).await;
    }
}

#[async_trait::async_trait]
impl ConsoleListenerMapHandler for Listener {
    async fn scanout_map(&mut self, scanout: ScanoutMap) {
        self.set_desktop_size(scanout.width, scanout.height).await;
        match scanout.mmap() {
            Ok(mmap) => self.inner.lock().unwrap().scanout_mmap = Some(mmap),
            Err(err) => warn!("Failed to mmap: {}", err),
        }
    }

    async fn update_map(&mut self, update: UpdateMap) {
        let update = {
            let inner = self.inner.lock().unwrap();
            let Some(mmap) = &inner.scanout_mmap else {
                warn!("Update with no map!");
                return;
            };
            let (stride, format) = (mmap.stride(), mmap.format());
            if format != 0x20020888 {
                warn!("Format not yet supported: {:X}", format);
                return;
            }
            // FIXME: use arc of data
            let data = mmap.as_ref()[update.y as usize * stride as usize + update.x as usize * 4..]
                .to_vec();
            Update {
                x: update.x,
                y: update.y,
                w: update.w,
                h: update.h,
                stride,
                format,
                data,
            }
        };
        self.update(update).await
    }
}

#[async_trait::async_trait]
impl ConsoleListenerHandler for Listener {
    async fn scanout(&mut self, scanout: Scanout) {
        self.set_desktop_size(scanout.width, scanout.height).await;

        self.update(Update {
            x: 0,
            y: 0,
            w: cast!(scanout.width),
            h: cast!(scanout.height),
            stride: scanout.stride,
            format: scanout.format,
            data: scanout.data,
        })
        .await
    }

    async fn update(&mut self, update: Update) {
        let Ok(format) = PixmanFormat(update.format).try_into() else {
            println!("Unhandled format {}", update.format);
            return;
        };

        let width: u16 = cast!(update.w);
        let height: u16 = cast!(update.h);

        let Some(stride) = NonZeroUsize::new(update.stride as usize) else {
            eprintln!("Error: zero stride");
            return;
        };
        let bitmap = DisplayUpdate::Bitmap(BitmapUpdate {
            // TODO: fix scary conversion
            x: cast!(update.x),
            y: cast!(update.y),
            width: cast!(width),
            height: cast!(height),
            format,
            data: update.data.into(),
            stride,
        });

        self.send(bitmap).await;
    }

    #[cfg(unix)]
    async fn scanout_dmabuf(&mut self, scanout: qemu_display::ScanoutDMABUF) {
        debug!(?scanout);
    }

    #[cfg(unix)]
    async fn update_dmabuf(&mut self, update: qemu_display::UpdateDMABUF) {
        debug!(?update);
    }

    async fn disable(&mut self) {
        debug!("disable");
    }

    async fn mouse_set(&mut self, set: MouseSet) {
        debug!(?set);

        // FIXME: this create weird effects on the client
        //
        // self.send(DisplayUpdate::PointerPosition(ironrdp_pdu::PointerPositionAttribute {
        //     x: cast!(set.x + self.cursor_hot.0),
        //     y: cast!(set.y + self.cursor_hot.1),
        // }))
        // .await;
        //

        if set.on == 0 {
            self.send(DisplayUpdate::HidePointer).await;
        }
    }

    async fn cursor_define(&mut self, cursor: Cursor) {
        debug!(?cursor);

        {
            let mut inner = self.inner.lock().unwrap();
            inner.cursor_hot = (cursor.hot_x, cursor.hot_y);
        }

        fn flip_vertically(image: Vec<u8>, width: usize, height: usize) -> Vec<u8> {
            let row_length = width * 4; // 4 bytes per pixel
            let mut flipped_image = vec![0; image.len()]; // Initialize a vector for the flipped image

            for y in 0..height {
                let source_row_start = y * row_length;
                let dest_row_start = (height - 1 - y) * row_length;

                // Copy the whole row from the source position to the destination position
                flipped_image[dest_row_start..dest_row_start + row_length]
                    .copy_from_slice(&image[source_row_start..source_row_start + row_length]);
            }

            flipped_image
        }

        let data = flip_vertically(cursor.data, cast!(cursor.width), cast!(cursor.height));

        self.send(DisplayUpdate::RGBAPointer(RGBAPointer {
            cache_index: 0,
            width: cast!(cursor.width),
            height: cast!(cursor.height),
            hot_x: cast!(cursor.hot_x),
            hot_y: cast!(cursor.hot_y),
            data,
        }))
        .await;
    }

    fn disconnected(&mut self) {
        debug!("console listener disconnected");
    }

    fn interfaces(&self) -> Vec<String> {
        if cfg!(unix) {
            vec!["org.qemu.Display1.Listener.Unix.Map".to_string()]
        } else {
            vec![]
        }
    }
}
