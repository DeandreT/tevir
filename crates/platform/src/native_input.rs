use domain::{Edge, Point, Size};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopGeometry {
    pub origin: Point,
    pub size: Size,
    pub monitor_count: u32,
}

#[derive(Debug)]
pub(crate) enum NativeCaptureEvent {
    Activated {
        edge: Edge,
        edge_position: Option<f64>,
    },
    #[cfg(target_os = "linux")]
    DesktopChanged(DesktopGeometry),
    Input(input_event::Event),
}

#[derive(Debug, Error)]
#[error("{0}")]
pub(crate) struct NativeInputError(String);

impl NativeInputError {
    fn message(error: impl std::fmt::Display) -> Self {
        Self(error.to_string())
    }

    fn context(operation: &str, error: impl std::fmt::Display) -> Self {
        Self(format!("{operation}: {error}"))
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::{
        collections::HashMap,
        num::NonZeroU32,
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, RwLock},
        time::{SystemTime, UNIX_EPOCH},
    };

    use ashpd::desktop::{
        CreateSessionOptions as RemoteCreateSessionOptions, Session,
        input_capture::{
            Activated, ActivatedBarrier, Barrier, BarrierID, Capabilities, ConnectToEISOptions,
            CreateSessionOptions, EnableOptions, GetZonesOptions, InputCapture, Region,
            ReleaseOptions, SetPointerBarriersOptions, Zones,
        },
        remote_desktop::{
            ConnectToEISOptions as RemoteConnectToEISOptions, DeviceType, RemoteDesktop,
            SelectDevicesOptions, StartOptions,
        },
    };
    use futures_util::StreamExt;
    use input_event::{Event, KeyboardEvent, PointerEvent};
    use reis::{
        ei::{
            self, Button, Keyboard, Pointer, PointerAbsolute, Scroll, button::ButtonState,
            handshake::ContextType, keyboard::KeyState,
        },
        event::{Connection, Device, DeviceCapability, EiEvent},
        tokio::EiConvertEventStream,
    };
    use tokio::{
        sync::mpsc,
        task::JoinHandle,
        time::{Duration, timeout},
    };

    use super::{DesktopGeometry, Edge, NativeCaptureEvent, NativeInputError, Point, Size};

    const NATIVE_EVENT_CAPACITY: usize = 256;
    const DEVICE_START_TIMEOUT: Duration = Duration::from_secs(10);
    const CAPABILITIES: &[DeviceCapability] = &[
        DeviceCapability::Pointer,
        DeviceCapability::PointerAbsolute,
        DeviceCapability::Keyboard,
        DeviceCapability::Scroll,
        DeviceCapability::Button,
    ];

    #[derive(Clone, Copy, Debug)]
    struct BarrierMeta {
        id: BarrierID,
        edge: Edge,
        position: (i32, i32, i32, i32),
    }

    #[derive(Clone, Copy, Debug)]
    struct ActiveCapture {
        activation_id: Option<u32>,
        cursor: (f32, f32),
        edge: Edge,
    }

    pub(crate) struct NativeCapture {
        portal: Arc<InputCapture>,
        session: Arc<Session<InputCapture>>,
        events: mpsc::Receiver<Result<NativeCaptureEvent, NativeInputError>>,
        geometry: Arc<RwLock<DesktopGeometry>>,
        active: Arc<Mutex<Option<ActiveCapture>>>,
        portal_task: JoinHandle<()>,
        eis_task: JoinHandle<()>,
    }

    impl NativeCapture {
        pub(crate) async fn new() -> Result<Self, NativeInputError> {
            let portal =
                Arc::new(InputCapture::new().await.map_err(|error| {
                    NativeInputError::context("open InputCapture portal", error)
                })?);
            let options = CreateSessionOptions::default()
                .set_capabilities(Capabilities::Keyboard | Capabilities::Pointer);
            let (session, _) = portal
                .create_session(None, options)
                .await
                .map_err(|error| NativeInputError::context("create InputCapture session", error))?;
            let session = Arc::new(session);
            let zones = load_zones(&portal, &session).await?;
            let geometry = Arc::new(RwLock::new(desktop_geometry(&zones)?));
            let barriers = install_barriers(&portal, &session, &zones).await?;
            let barrier_map = Arc::new(RwLock::new(
                barriers
                    .iter()
                    .map(|barrier| (barrier.id, *barrier))
                    .collect(),
            ));

            let fd = portal
                .connect_to_eis(&session, ConnectToEISOptions::default())
                .await
                .map_err(|error| NativeInputError::context("connect InputCapture to EIS", error))?;
            let stream = UnixStream::from(fd);
            stream
                .set_nonblocking(true)
                .map_err(|error| NativeInputError::context("configure EIS socket", error))?;
            let context = ei::Context::new(stream)
                .map_err(|error| NativeInputError::context("create EIS context", error))?;
            let (connection, eis_events) = context
                .handshake_tokio("io.github.tevir", ContextType::Receiver)
                .await
                .map_err(|error| NativeInputError::context("complete EIS handshake", error))?;

            let (event_tx, events) = mpsc::channel(NATIVE_EVENT_CAPACITY);
            let active = Arc::new(Mutex::new(None));
            let portal_task = tokio::task::spawn_local(run_portal_events(
                portal.clone(),
                session.clone(),
                event_tx.clone(),
                barrier_map,
                geometry.clone(),
                active.clone(),
            ));
            let eis_task = tokio::task::spawn_local(run_capture_eis(
                context, connection, eis_events, event_tx,
            ));
            if let Err(error) = portal.enable(&session, EnableOptions::default()).await {
                portal_task.abort();
                eis_task.abort();
                let _ = session.close().await;
                return Err(NativeInputError::context(
                    "enable InputCapture session",
                    error,
                ));
            }

            Ok(Self {
                portal,
                session,
                events,
                geometry,
                active,
                portal_task,
                eis_task,
            })
        }

        pub(crate) async fn next(
            &mut self,
        ) -> Option<Result<NativeCaptureEvent, NativeInputError>> {
            self.events.recv().await
        }

        pub(crate) async fn create(&mut self, _edge: Edge) -> Result<(), NativeInputError> {
            Ok(())
        }

        pub(crate) fn desktop_geometry(&self) -> Option<DesktopGeometry> {
            Some(
                self.geometry
                    .read()
                    .map_or_else(|poisoned| *poisoned.into_inner(), |geometry| *geometry),
            )
        }

        pub(crate) async fn release(&mut self) -> Result<(), NativeInputError> {
            let active = self.active.lock().map_or_else(
                |poisoned| poisoned.into_inner().take(),
                |mut active| active.take(),
            );
            let Some(active) = active else {
                return Ok(());
            };
            let cursor = release_position(active);
            let options = ReleaseOptions::default()
                .set_activation_id(active.activation_id)
                .set_cursor_position(Some(cursor));
            self.portal
                .release(&self.session, options)
                .await
                .map_err(|error| NativeInputError::context("release InputCapture session", error))
        }

        pub(crate) async fn terminate(&mut self) -> Result<(), NativeInputError> {
            let _ = self.release().await;
            self.portal_task.abort();
            self.eis_task.abort();
            self.session
                .close()
                .await
                .map_err(|error| NativeInputError::context("close InputCapture session", error))
        }
    }

    async fn load_zones(
        portal: &InputCapture,
        session: &Session<InputCapture>,
    ) -> Result<Zones, NativeInputError> {
        portal
            .zones(session, GetZonesOptions::default())
            .await
            .map_err(|error| NativeInputError::context("request InputCapture zones", error))?
            .response()
            .map_err(|error| NativeInputError::context("read InputCapture zones", error))
    }

    async fn install_barriers(
        portal: &InputCapture,
        session: &Session<InputCapture>,
        zones: &Zones,
    ) -> Result<Vec<BarrierMeta>, NativeInputError> {
        let mut next_id = NonZeroU32::MIN;
        let mut metadata = Vec::new();
        for region in zones.regions() {
            for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
                let id = next_id;
                next_id = next_id.checked_add(1).ok_or_else(|| {
                    NativeInputError::message("InputCapture barrier IDs exhausted")
                })?;
                metadata.push(BarrierMeta {
                    id,
                    edge,
                    position: barrier_position(*region, edge),
                });
            }
        }
        let barriers = metadata
            .iter()
            .map(|barrier| Barrier::new(barrier.id, barrier.position))
            .collect::<Vec<_>>();
        let response = portal
            .set_pointer_barriers(
                session,
                &barriers,
                zones.zone_set(),
                SetPointerBarriersOptions::default(),
            )
            .await
            .map_err(|error| NativeInputError::context("set InputCapture barriers", error))?
            .response()
            .map_err(|error| NativeInputError::context("read InputCapture barriers", error))?;
        let failed = response.failed_barriers();
        metadata.retain(|barrier| !failed.contains(&barrier.id));
        if metadata.is_empty() {
            return Err(NativeInputError::message(
                "the compositor rejected every InputCapture barrier",
            ));
        }
        Ok(metadata)
    }

    async fn run_portal_events(
        portal: Arc<InputCapture>,
        session: Arc<Session<InputCapture>>,
        events: mpsc::Sender<Result<NativeCaptureEvent, NativeInputError>>,
        barriers: Arc<RwLock<HashMap<BarrierID, BarrierMeta>>>,
        geometry: Arc<RwLock<DesktopGeometry>>,
        active: Arc<Mutex<Option<ActiveCapture>>>,
    ) {
        let result: Result<(), NativeInputError> = async {
            let mut activations = portal.receive_activated().await.map_err(|error| {
                NativeInputError::context("subscribe to InputCapture activations", error)
            })?;
            let mut zone_changes = portal.receive_zones_changed().await.map_err(|error| {
                NativeInputError::context("subscribe to InputCapture zone changes", error)
            })?;
            loop {
                tokio::select! {
                    activation = activations.next() => {
                        let Some(activation) = activation else {
                            return Err(NativeInputError::message(
                                "InputCapture activation stream ended",
                            ));
                        };
                        handle_activation(activation, &barriers, &geometry, &active, &events).await?;
                    }
                    changed = zone_changes.next() => {
                        let Some(_) = changed else {
                            return Err(NativeInputError::message(
                                "InputCapture zone-change stream ended",
                            ));
                        };
                        let zones = load_zones(&portal, &session).await?;
                        let next_geometry = desktop_geometry(&zones)?;
                        let next_barriers = install_barriers(&portal, &session, &zones).await?;
                        if let Ok(mut value) = geometry.write() {
                            *value = next_geometry;
                        }
                        if let Ok(mut value) = barriers.write() {
                            *value = next_barriers
                                .into_iter()
                                .map(|barrier| (barrier.id, barrier))
                                .collect();
                        }
                        events
                            .send(Ok(NativeCaptureEvent::DesktopChanged(next_geometry)))
                            .await
                            .map_err(|_| {
                                NativeInputError::message("native capture receiver stopped")
                            })?;
                    }
                }
            }
        }
        .await;
        if let Err(error) = result {
            let _ = events.send(Err(error)).await;
        }
    }

    async fn handle_activation(
        activation: Activated,
        barriers: &RwLock<HashMap<BarrierID, BarrierMeta>>,
        geometry: &RwLock<DesktopGeometry>,
        active: &Mutex<Option<ActiveCapture>>,
        events: &mpsc::Sender<Result<NativeCaptureEvent, NativeInputError>>,
    ) -> Result<(), NativeInputError> {
        let cursor = activation.cursor_position().ok_or_else(|| {
            NativeInputError::message("the compositor did not report the activation position")
        })?;
        let barrier = {
            let barriers = barriers
                .read()
                .map_err(|_| NativeInputError::message("InputCapture barrier state is poisoned"))?;
            match activation.barrier_id() {
                Some(ActivatedBarrier::Barrier(id)) => barriers.get(&id).copied(),
                Some(ActivatedBarrier::UnknownBarrier) | None => {
                    barriers.values().copied().min_by(|first, second| {
                        barrier_distance(first.position, cursor)
                            .total_cmp(&barrier_distance(second.position, cursor))
                    })
                }
            }
        }
        .ok_or_else(|| NativeInputError::message("activated InputCapture barrier is unknown"))?;
        let edge_position = {
            let geometry = geometry.read().map_err(|_| {
                NativeInputError::message("InputCapture geometry state is poisoned")
            })?;
            normalized_edge_position(*geometry, barrier.edge, cursor)
        };

        if let Ok(mut value) = active.lock() {
            *value = Some(ActiveCapture {
                activation_id: activation.activation_id(),
                cursor,
                edge: barrier.edge,
            });
        }
        events
            .send(Ok(NativeCaptureEvent::Activated {
                edge: barrier.edge,
                edge_position: Some(edge_position),
            }))
            .await
            .map_err(|_| NativeInputError::message("native capture receiver stopped"))
    }

    async fn run_capture_eis(
        context: ei::Context,
        _connection: Connection,
        mut input: EiConvertEventStream,
        events: mpsc::Sender<Result<NativeCaptureEvent, NativeInputError>>,
    ) {
        while let Some(event) = input.next().await {
            let event = match event {
                Ok(event) => event,
                Err(error) => {
                    let _ = events
                        .send(Err(NativeInputError::context(
                            "receive captured EIS input",
                            error,
                        )))
                        .await;
                    return;
                }
            };
            if let EiEvent::SeatAdded(seat) = &event {
                seat.seat.bind_capabilities(CAPABILITIES);
                if let Err(error) = context.flush() {
                    let _ = events
                        .send(Err(NativeInputError::context(
                            "bind captured EIS devices",
                            error,
                        )))
                        .await;
                    return;
                }
            }
            if let EiEvent::Disconnected(disconnected) = &event {
                let _ = events
                    .send(Err(NativeInputError::message(format!(
                        "captured EIS connection closed: {}",
                        disconnected.explanation
                    ))))
                    .await;
                return;
            }
            for event in Event::from_ei_event(event) {
                if events
                    .send(Ok(NativeCaptureEvent::Input(event)))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
        let _ = events
            .send(Err(NativeInputError::message(
                "captured EIS event stream ended",
            )))
            .await;
    }

    fn desktop_geometry(zones: &Zones) -> Result<DesktopGeometry, NativeInputError> {
        geometry_from_rectangles(zones.regions().iter().map(|region| {
            (
                i64::from(region.x_offset()),
                i64::from(region.y_offset()),
                u64::from(region.width()),
                u64::from(region.height()),
            )
        }))
    }

    fn geometry_from_device(device: &Device) -> Result<DesktopGeometry, NativeInputError> {
        geometry_from_rectangles(device.regions().iter().map(|region| {
            (
                i64::from(region.x),
                i64::from(region.y),
                u64::from(region.width),
                u64::from(region.height),
            )
        }))
    }

    fn geometry_from_rectangles(
        rectangles: impl Iterator<Item = (i64, i64, u64, u64)>,
    ) -> Result<DesktopGeometry, NativeInputError> {
        let rectangles = rectangles.collect::<Vec<_>>();
        let Some(&(first_x, first_y, first_width, first_height)) = rectangles.first() else {
            return Err(NativeInputError::message(
                "the compositor reported no desktop regions",
            ));
        };
        let mut left = first_x;
        let mut top = first_y;
        let mut right = first_x.saturating_add_unsigned(first_width);
        let mut bottom = first_y.saturating_add_unsigned(first_height);
        for &(x, y, width, height) in &rectangles[1..] {
            left = left.min(x);
            top = top.min(y);
            right = right.max(x.saturating_add_unsigned(width));
            bottom = bottom.max(y.saturating_add_unsigned(height));
        }
        let width = u32::try_from(right.saturating_sub(left))
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or_else(|| {
                NativeInputError::message("desktop width exceeds the supported range")
            })?;
        let height = u32::try_from(bottom.saturating_sub(top))
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or_else(|| {
                NativeInputError::message("desktop height exceeds the supported range")
            })?;
        Ok(DesktopGeometry {
            origin: Point {
                x: i32::try_from(left).map_err(|_| {
                    NativeInputError::message("desktop X origin exceeds the supported range")
                })?,
                y: i32::try_from(top).map_err(|_| {
                    NativeInputError::message("desktop Y origin exceeds the supported range")
                })?,
            },
            size: Size::new(width, height),
            monitor_count: u32::try_from(rectangles.len()).unwrap_or(u32::MAX),
        })
    }

    fn barrier_position(region: Region, edge: Edge) -> (i32, i32, i32, i32) {
        let left = region.x_offset();
        let top = region.y_offset();
        let right = left.saturating_add_unsigned(region.width());
        let bottom = top.saturating_add_unsigned(region.height());
        match edge {
            Edge::Left => (left, top, left, bottom.saturating_sub(1)),
            Edge::Right => (right, top, right, bottom.saturating_sub(1)),
            Edge::Top => (left, top, right.saturating_sub(1), top),
            Edge::Bottom => (left, bottom, right.saturating_sub(1), bottom),
        }
    }

    fn barrier_distance(position: (i32, i32, i32, i32), cursor: (f32, f32)) -> f32 {
        let (x1, y1, x2, y2) = position;
        let vx = (x2 - x1) as f32;
        let vy = (y2 - y1) as f32;
        let length_squared = vx.mul_add(vx, vy * vy);
        if length_squared == 0.0 {
            return (cursor.0 - x1 as f32).hypot(cursor.1 - y1 as f32);
        }
        let projection = (((cursor.0 - x1 as f32) * vx + (cursor.1 - y1 as f32) * vy)
            / length_squared)
            .clamp(0.0, 1.0);
        let closest_x = (vx * projection).mul_add(1.0, x1 as f32);
        let closest_y = (vy * projection).mul_add(1.0, y1 as f32);
        (cursor.0 - closest_x).hypot(cursor.1 - closest_y)
    }

    fn normalized_edge_position(geometry: DesktopGeometry, edge: Edge, cursor: (f32, f32)) -> f64 {
        let (value, start, length) = match edge {
            Edge::Left | Edge::Right => (
                f64::from(cursor.1),
                f64::from(geometry.origin.y),
                f64::from(geometry.size.height.get()),
            ),
            Edge::Top | Edge::Bottom => (
                f64::from(cursor.0),
                f64::from(geometry.origin.x),
                f64::from(geometry.size.width.get()),
            ),
        };
        ((value - start) / length.max(1.0)).clamp(0.0, 1.0)
    }

    fn release_position(active: ActiveCapture) -> (f64, f64) {
        let (dx, dy) = match active.edge {
            Edge::Left => (1.0, 0.0),
            Edge::Right => (-1.0, 0.0),
            Edge::Top => (0.0, 1.0),
            Edge::Bottom => (0.0, -1.0),
        };
        (
            f64::from(active.cursor.0) + dx,
            f64::from(active.cursor.1) + dy,
        )
    }

    #[derive(Default)]
    struct InjectionDevices {
        pointer: Option<(ei::Device, Pointer)>,
        pointer_absolute: Option<(ei::Device, PointerAbsolute)>,
        keyboard: Option<(ei::Device, Keyboard)>,
        button: Option<(ei::Device, Button)>,
        scroll: Option<(ei::Device, Scroll)>,
        geometry: Option<DesktopGeometry>,
    }

    impl InjectionDevices {
        fn bind(&mut self, device: &Device) -> Result<Option<DesktopGeometry>, NativeInputError> {
            let native = device.device().clone();
            if let Some(pointer) = device.interface::<Pointer>() {
                self.pointer = Some((native.clone(), pointer));
            }
            let mut geometry = None;
            if let Some(pointer_absolute) = device.interface::<PointerAbsolute>() {
                let next_geometry = geometry_from_device(device)?;
                self.pointer_absolute = Some((native.clone(), pointer_absolute));
                self.geometry = Some(next_geometry);
                geometry = Some(next_geometry);
            }
            if let Some(keyboard) = device.interface::<Keyboard>() {
                self.keyboard = Some((native.clone(), keyboard));
            }
            if let Some(button) = device.interface::<Button>() {
                self.button = Some((native.clone(), button));
            }
            if let Some(scroll) = device.interface::<Scroll>() {
                self.scroll = Some((native, scroll));
            }
            Ok(geometry)
        }

        fn is_ready(&self) -> bool {
            self.pointer.is_some()
                && self.pointer_absolute.is_some()
                && self.keyboard.is_some()
                && self.button.is_some()
                && self.scroll.is_some()
                && self.geometry.is_some()
        }
    }

    enum InjectionNativeEvent {
        Ready(DesktopGeometry),
        DisplayChanged(DesktopGeometry),
        Failed(NativeInputError),
    }

    pub(crate) struct NativeInjection {
        portal: RemoteDesktop,
        session: Session<RemoteDesktop>,
        context: ei::Context,
        connection: Connection,
        devices: Arc<RwLock<InjectionDevices>>,
        events: mpsc::Receiver<InjectionNativeEvent>,
        event_task: JoinHandle<()>,
        geometry: DesktopGeometry,
    }

    impl NativeInjection {
        pub(crate) async fn new() -> Result<Self, NativeInputError> {
            let portal = RemoteDesktop::new()
                .await
                .map_err(|error| NativeInputError::context("open RemoteDesktop portal", error))?;
            let session = portal
                .create_session(RemoteCreateSessionOptions::default())
                .await
                .map_err(|error| {
                    NativeInputError::context("create RemoteDesktop session", error)
                })?;
            portal
                .select_devices(
                    &session,
                    SelectDevicesOptions::default()
                        .set_devices(DeviceType::Keyboard | DeviceType::Pointer),
                )
                .await
                .map_err(|error| NativeInputError::context("select RemoteDesktop devices", error))?
                .response()
                .map_err(|error| {
                    NativeInputError::context("authorize RemoteDesktop devices", error)
                })?;
            portal
                .start(&session, None, StartOptions::default())
                .await
                .map_err(|error| NativeInputError::context("start RemoteDesktop session", error))?
                .response()
                .map_err(|error| {
                    NativeInputError::context("authorize RemoteDesktop session", error)
                })?;
            let fd = portal
                .connect_to_eis(&session, RemoteConnectToEISOptions::default())
                .await
                .map_err(|error| {
                    NativeInputError::context("connect RemoteDesktop to EIS", error)
                })?;
            let stream = UnixStream::from(fd);
            stream
                .set_nonblocking(true)
                .map_err(|error| NativeInputError::context("configure EIS socket", error))?;
            let context = ei::Context::new(stream)
                .map_err(|error| NativeInputError::context("create EIS context", error))?;
            let (connection, input) = context
                .handshake_tokio("io.github.tevir", ContextType::Sender)
                .await
                .map_err(|error| NativeInputError::context("complete EIS handshake", error))?;
            let devices = Arc::new(RwLock::new(InjectionDevices::default()));
            let (event_tx, mut events) = mpsc::channel(NATIVE_EVENT_CAPACITY);
            let event_task = tokio::task::spawn_local(run_injection_eis(
                context.clone(),
                input,
                devices.clone(),
                event_tx,
            ));
            let geometry = match timeout(DEVICE_START_TIMEOUT, events.recv()).await {
                Ok(Some(InjectionNativeEvent::Ready(geometry))) => geometry,
                Ok(Some(InjectionNativeEvent::Failed(error))) => return Err(error),
                Ok(Some(InjectionNativeEvent::DisplayChanged(_))) => {
                    return Err(NativeInputError::message(
                        "EIS display changed before input devices became ready",
                    ));
                }
                Ok(None) => {
                    return Err(NativeInputError::message(
                        "EIS input device discovery stopped",
                    ));
                }
                Err(_) => {
                    return Err(NativeInputError::message(
                        "timed out waiting for EIS input devices",
                    ));
                }
            };
            Ok(Self {
                portal,
                session,
                context,
                connection,
                devices,
                events,
                event_task,
                geometry,
            })
        }

        pub(crate) fn desktop_geometry(&self) -> Option<DesktopGeometry> {
            Some(self.geometry)
        }

        pub(crate) fn try_display_change(
            &mut self,
        ) -> Result<Option<DesktopGeometry>, NativeInputError> {
            match self.events.try_recv() {
                Ok(InjectionNativeEvent::DisplayChanged(geometry)) => {
                    self.geometry = geometry;
                    Ok(Some(geometry))
                }
                Ok(InjectionNativeEvent::Failed(error)) => Err(error),
                Ok(InjectionNativeEvent::Ready(_)) | Err(mpsc::error::TryRecvError::Empty) => {
                    Ok(None)
                }
                Err(mpsc::error::TryRecvError::Disconnected) => Err(NativeInputError::message(
                    "EIS input device monitor stopped",
                )),
            }
        }

        pub(crate) async fn consume(
            &mut self,
            event: Event,
            _handle: u64,
        ) -> Result<(), NativeInputError> {
            let now = event_time();
            let devices = self
                .devices
                .read()
                .map_err(|_| NativeInputError::message("EIS input device state is poisoned"))?;
            match event {
                Event::Pointer(PointerEvent::Motion { dx, dy, .. }) => {
                    let (device, pointer) = devices
                        .pointer
                        .as_ref()
                        .ok_or_else(|| NativeInputError::message("EIS pointer is unavailable"))?;
                    pointer.motion_relative(dx as f32, dy as f32);
                    device.frame(self.connection.serial(), now);
                }
                Event::Pointer(PointerEvent::Button { button, state, .. }) => {
                    let (device, interface) = devices
                        .button
                        .as_ref()
                        .ok_or_else(|| NativeInputError::message("EIS buttons are unavailable"))?;
                    interface.button(
                        button,
                        if state == 0 {
                            ButtonState::Released
                        } else {
                            ButtonState::Press
                        },
                    );
                    device.frame(self.connection.serial(), now);
                }
                Event::Pointer(PointerEvent::Axis { axis, value, .. }) => {
                    let (device, scroll) = devices
                        .scroll
                        .as_ref()
                        .ok_or_else(|| NativeInputError::message("EIS scrolling is unavailable"))?;
                    if axis == 0 {
                        scroll.scroll(0.0, value as f32);
                    } else {
                        scroll.scroll(value as f32, 0.0);
                    }
                    device.frame(self.connection.serial(), now);
                }
                Event::Pointer(PointerEvent::AxisDiscrete120 { axis, value }) => {
                    let (device, scroll) = devices
                        .scroll
                        .as_ref()
                        .ok_or_else(|| NativeInputError::message("EIS scrolling is unavailable"))?;
                    if axis == 0 {
                        scroll.scroll_discrete(0, value);
                    } else {
                        scroll.scroll_discrete(value, 0);
                    }
                    device.frame(self.connection.serial(), now);
                }
                Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => {
                    let (device, keyboard) = devices
                        .keyboard
                        .as_ref()
                        .ok_or_else(|| NativeInputError::message("EIS keyboard is unavailable"))?;
                    keyboard.key(
                        key,
                        if state == 0 {
                            KeyState::Released
                        } else {
                            KeyState::Press
                        },
                    );
                    device.frame(self.connection.serial(), now);
                }
                Event::Keyboard(KeyboardEvent::Modifiers { .. }) => {}
            }
            drop(devices);
            self.context
                .flush()
                .map_err(|error| NativeInputError::context("flush EIS input", error))
        }

        pub(crate) async fn warp_cursor(
            &mut self,
            position: Point,
        ) -> Result<(), NativeInputError> {
            let devices = self
                .devices
                .read()
                .map_err(|_| NativeInputError::message("EIS input device state is poisoned"))?;
            let (device, pointer) = devices
                .pointer_absolute
                .as_ref()
                .ok_or_else(|| NativeInputError::message("EIS absolute pointer is unavailable"))?;
            let x = i64::from(self.geometry.origin.x) + i64::from(position.x);
            let y = i64::from(self.geometry.origin.y) + i64::from(position.y);
            pointer.motion_absolute(x as f32, y as f32);
            device.frame(self.connection.serial(), event_time());
            drop(devices);
            self.context
                .flush()
                .map_err(|error| NativeInputError::context("warp EIS pointer", error))
        }

        pub(crate) async fn release_keys(&mut self, _handle: u64) -> Result<(), NativeInputError> {
            Ok(())
        }

        pub(crate) async fn terminate(&mut self) -> Result<(), NativeInputError> {
            self.event_task.abort();
            self.session
                .close()
                .await
                .map_err(|error| NativeInputError::context("close RemoteDesktop session", error))?;
            let _ = &self.portal;
            Ok(())
        }
    }

    async fn run_injection_eis(
        context: ei::Context,
        mut input: EiConvertEventStream,
        devices: Arc<RwLock<InjectionDevices>>,
        events: mpsc::Sender<InjectionNativeEvent>,
    ) {
        let mut ready = false;
        while let Some(event) = input.next().await {
            let event = match event {
                Ok(event) => event,
                Err(error) => {
                    let _ = events
                        .send(InjectionNativeEvent::Failed(NativeInputError::context(
                            "receive emulated EIS event",
                            error,
                        )))
                        .await;
                    return;
                }
            };
            let mut display_change = None;
            match event {
                EiEvent::SeatAdded(seat) => seat.seat.bind_capabilities(CAPABILITIES),
                EiEvent::DeviceAdded(added) => {
                    let result = devices
                        .write()
                        .map_err(|_| {
                            NativeInputError::message("EIS input device state is poisoned")
                        })
                        .and_then(|mut devices| devices.bind(&added.device));
                    match result {
                        Ok(geometry) => display_change = geometry,
                        Err(error) => {
                            let _ = events.send(InjectionNativeEvent::Failed(error)).await;
                            return;
                        }
                    }
                }
                EiEvent::DeviceResumed(resumed) => {
                    resumed.device.device().start_emulating(resumed.serial, 0);
                }
                EiEvent::Disconnected(disconnected) => {
                    let _ = events
                        .send(InjectionNativeEvent::Failed(NativeInputError::message(
                            format!(
                                "emulated EIS connection closed: {}",
                                disconnected.explanation
                            ),
                        )))
                        .await;
                    return;
                }
                EiEvent::SeatRemoved(_)
                | EiEvent::DeviceRemoved(_)
                | EiEvent::DevicePaused(_)
                | EiEvent::KeyboardModifiers(_)
                | EiEvent::Frame(_)
                | EiEvent::DeviceStartEmulating(_)
                | EiEvent::DeviceStopEmulating(_)
                | EiEvent::PointerMotion(_)
                | EiEvent::PointerMotionAbsolute(_)
                | EiEvent::Button(_)
                | EiEvent::ScrollDelta(_)
                | EiEvent::ScrollStop(_)
                | EiEvent::ScrollCancel(_)
                | EiEvent::ScrollDiscrete(_)
                | EiEvent::KeyboardKey(_)
                | EiEvent::TouchDown(_)
                | EiEvent::TouchUp(_)
                | EiEvent::TouchMotion(_)
                | EiEvent::TouchCancel(_) => {}
            }
            if let Err(error) = context.flush() {
                let _ = events
                    .send(InjectionNativeEvent::Failed(NativeInputError::context(
                        "bind emulated EIS devices",
                        error,
                    )))
                    .await;
                return;
            }
            let state = match devices.read() {
                Ok(devices) => (devices.is_ready(), devices.geometry),
                Err(_) => {
                    let _ = events
                        .send(InjectionNativeEvent::Failed(NativeInputError::message(
                            "EIS input device state is poisoned",
                        )))
                        .await;
                    return;
                }
            };
            if !ready && state.0 {
                ready = true;
                if events
                    .send(InjectionNativeEvent::Ready(state.1.unwrap_or(
                        DesktopGeometry {
                            origin: Point { x: 0, y: 0 },
                            size: Size::new(NonZeroU32::MIN, NonZeroU32::MIN),
                            monitor_count: 1,
                        },
                    )))
                    .await
                    .is_err()
                {
                    return;
                }
            } else if ready
                && let Some(geometry) = display_change
                && events
                    .send(InjectionNativeEvent::DisplayChanged(geometry))
                    .await
                    .is_err()
            {
                return;
            }
        }
        let _ = events
            .send(InjectionNativeEvent::Failed(NativeInputError::message(
                "emulated EIS event stream ended",
            )))
            .await;
    }

    fn event_time() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
            })
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use enigo::{Coordinate, Enigo, Mouse, Settings};
    use futures_util::StreamExt;

    use super::{DesktopGeometry, Edge, NativeCaptureEvent, NativeInputError, Point};

    const ENGINE_HANDLE: u64 = 1;

    pub(crate) struct NativeCapture {
        capture: capture_engine::InputCapture,
        cursor: Enigo,
    }

    impl NativeCapture {
        pub(crate) async fn new() -> Result<Self, NativeInputError> {
            let capture = capture_engine::InputCapture::new(Some(capture_engine::Backend::Windows))
                .await
                .map_err(|error| NativeInputError::context("open Windows input capture", error))?;
            let cursor = Enigo::new(&Settings::default())
                .map_err(|error| NativeInputError::context("open Windows pointer", error))?;
            Ok(Self { capture, cursor })
        }

        pub(crate) async fn create(&mut self, edge: Edge) -> Result<(), NativeInputError> {
            self.capture
                .create(capture_handle(edge), capture_position(edge))
                .await
                .map_err(|error| NativeInputError::context("create Windows capture edge", error))
        }

        pub(crate) async fn next(
            &mut self,
        ) -> Option<Result<NativeCaptureEvent, NativeInputError>> {
            let event = self.capture.next().await?;
            Some(match event {
                Ok((handle, capture_engine::CaptureEvent::Begin)) => {
                    let edge = capture_edge(handle).ok_or_else(|| {
                        NativeInputError::message(format!("unknown capture handle {handle}"))
                    });
                    edge.and_then(|edge| {
                        let edge_position = self.cursor.location().ok().and_then(|position| {
                            normalized_windows_position(&self.cursor, edge, position)
                        });
                        Ok(NativeCaptureEvent::Activated {
                            edge,
                            edge_position,
                        })
                    })
                }
                Ok((_, capture_engine::CaptureEvent::Input(event))) => {
                    Ok(NativeCaptureEvent::Input(event))
                }
                Err(error) => Err(NativeInputError::context("capture Windows input", error)),
            })
        }

        pub(crate) fn desktop_geometry(&self) -> Option<DesktopGeometry> {
            None
        }

        pub(crate) async fn release(&mut self) -> Result<(), NativeInputError> {
            self.capture
                .release()
                .await
                .map_err(|error| NativeInputError::context("release Windows input capture", error))
        }

        pub(crate) async fn terminate(&mut self) -> Result<(), NativeInputError> {
            self.capture
                .terminate()
                .await
                .map_err(|error| NativeInputError::context("stop Windows input capture", error))
        }
    }

    pub(crate) struct NativeInjection {
        injection: emulation_engine::InputEmulation,
        cursor: Enigo,
    }

    impl NativeInjection {
        pub(crate) async fn new() -> Result<Self, NativeInputError> {
            let mut injection =
                emulation_engine::InputEmulation::new(Some(emulation_engine::Backend::Windows))
                    .await
                    .map_err(|error| {
                        NativeInputError::context("open Windows input injection", error)
                    })?;
            let _ = injection.create(ENGINE_HANDLE).await;
            let cursor = Enigo::new(&Settings::default())
                .map_err(|error| NativeInputError::context("open Windows pointer", error))?;
            Ok(Self { injection, cursor })
        }

        pub(crate) fn desktop_geometry(&self) -> Option<DesktopGeometry> {
            None
        }

        pub(crate) fn try_display_change(
            &mut self,
        ) -> Result<Option<DesktopGeometry>, NativeInputError> {
            Ok(None)
        }

        pub(crate) async fn consume(
            &mut self,
            event: input_event::Event,
            handle: u64,
        ) -> Result<(), NativeInputError> {
            self.injection
                .consume(event, handle)
                .await
                .map_err(|error| NativeInputError::context("inject Windows input", error))
        }

        pub(crate) async fn warp_cursor(
            &mut self,
            position: Point,
        ) -> Result<(), NativeInputError> {
            self.cursor
                .move_mouse(position.x, position.y, Coordinate::Abs)
                .map_err(|error| NativeInputError::context("warp Windows pointer", error))
        }

        pub(crate) async fn release_keys(&mut self, handle: u64) -> Result<(), NativeInputError> {
            self.injection
                .release_keys(handle)
                .await
                .map_err(|error| NativeInputError::context("release Windows keys", error))
        }

        pub(crate) async fn terminate(&mut self) -> Result<(), NativeInputError> {
            self.injection.destroy(ENGINE_HANDLE).await;
            self.injection.terminate().await;
            Ok(())
        }
    }

    fn normalized_windows_position(
        cursor: &Enigo,
        edge: Edge,
        position: (i32, i32),
    ) -> Option<f64> {
        let (width, height) = cursor.main_display().ok()?;
        let (value, length) = match edge {
            Edge::Left | Edge::Right => (position.1, height),
            Edge::Top | Edge::Bottom => (position.0, width),
        };
        (length > 0).then(|| (f64::from(value) / f64::from(length)).clamp(0.0, 1.0))
    }

    const fn capture_position(edge: Edge) -> capture_engine::Position {
        match edge {
            Edge::Left => capture_engine::Position::Left,
            Edge::Right => capture_engine::Position::Right,
            Edge::Top => capture_engine::Position::Top,
            Edge::Bottom => capture_engine::Position::Bottom,
        }
    }

    const fn capture_handle(edge: Edge) -> u64 {
        match edge {
            Edge::Left => 1,
            Edge::Right => 2,
            Edge::Top => 3,
            Edge::Bottom => 4,
        }
    }

    const fn capture_edge(handle: u64) -> Option<Edge> {
        match handle {
            1 => Some(Edge::Left),
            2 => Some(Edge::Right),
            3 => Some(Edge::Top),
            4 => Some(Edge::Bottom),
            _ => None,
        }
    }
}

pub(crate) use platform::{NativeCapture, NativeInjection};
