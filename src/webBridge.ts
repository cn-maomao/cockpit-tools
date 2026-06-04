type BridgeInvokeResponse =
  | { ok: true; value: unknown }
  | { ok: false; error: unknown };

type CallbackEntry = {
  callback: (...args: unknown[]) => void;
  once: boolean;
};

type EventListenerEntry = {
  event: string;
  handlerId: number;
};

type WebEventMessage = {
  sequence: number;
  event: string;
  payload: unknown;
};

type WebEventPollResponse = {
  events?: WebEventMessage[];
  latestSequence?: number;
};

const bridgeAnyWindow = window as unknown as {
  __TAURI_INTERNALS__?: {
    metadata: {
      currentWindow: { label: string };
      currentWebview: { label: string };
    };
    invoke: (cmd: string, args?: Record<string, unknown>, options?: unknown) => Promise<unknown>;
    transformCallback: (callback: (...args: unknown[]) => void, once?: boolean) => number;
    runCallback: (id: number, ...args: unknown[]) => void;
    unregisterCallback: (id: number) => void;
  };
  __TAURI_EVENT_PLUGIN_INTERNALS__?: {
    unregisterListener: (event: string, eventId: number) => void;
  };
};

if (!bridgeAnyWindow.__TAURI_INTERNALS__) {
  const callbacks = new Map<number, CallbackEntry>();
  const eventListeners = new Map<number, EventListenerEntry>();
  let nextCallbackId = 1;
  let eventPumpStarted = false;
  let lastEventSequence = 0;

  const callBridge = async (cmd: string, args: Record<string, unknown> = {}) => {
    const response = await fetch('/__cockpit_web__/invoke', {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
      },
      body: JSON.stringify({ cmd, args }),
    });

    const payload = (await response.json()) as BridgeInvokeResponse;
    if (!response.ok || !payload.ok) {
      const rawError = payload && 'error' in payload ? payload.error : response.statusText;
      throw rawError instanceof Error ? rawError : new Error(String(rawError || 'Web invoke failed'));
    }
    return payload.value;
  };

  const getDialogButtonLabels = (buttons: unknown) => {
    if (typeof buttons === 'string') {
      switch (buttons) {
        case 'OkCancel':
          return { ok: 'Ok', cancel: 'Cancel', confirm: true };
        case 'YesNo':
          return { ok: 'Yes', cancel: 'No', confirm: true };
        case 'YesNoCancel':
          return { ok: 'Yes', cancel: 'No', confirm: true };
        case 'Ok':
        default:
          return { ok: 'Ok', confirm: false };
      }
    }

    if (buttons && typeof buttons === 'object') {
      const value = buttons as Record<string, unknown>;
      const okCancel = value.OkCancelCustom;
      if (Array.isArray(okCancel)) {
        return { ok: String(okCancel[0] ?? 'Ok'), cancel: String(okCancel[1] ?? 'Cancel'), confirm: true };
      }
      const yesNoCancel = value.YesNoCancelCustom;
      if (Array.isArray(yesNoCancel)) {
        return { ok: String(yesNoCancel[0] ?? 'Yes'), cancel: String(yesNoCancel[1] ?? 'No'), confirm: true };
      }
      if ('OkCustom' in value) {
        return { ok: String(value.OkCustom ?? 'Ok'), confirm: false };
      }
    }

    return { ok: 'Ok', confirm: false };
  };

  const formatDialogMessage = (args: Record<string, unknown>) => {
    const message = String(args.message ?? '');
    const title = args.title ? String(args.title) : '';
    return title ? `${title}\n\n${message}` : message;
  };

  const dispatchWebEvent = (message: WebEventMessage) => {
    for (const [eventId, listener] of eventListeners) {
      if (listener.event !== message.event) continue;
      const entry = callbacks.get(listener.handlerId);
      if (!entry) continue;
      entry.callback({ event: message.event, payload: message.payload, id: eventId });
      if (entry.once) {
        callbacks.delete(listener.handlerId);
        eventListeners.delete(eventId);
        void callBridge('plugin:event|unlisten', { event: listener.event, eventId }).catch(() => {});
      }
    }
  };

  const pollEvents = async () => {
    while (true) {
      try {
        const response = await fetch(`/__cockpit_web__/events?after=${lastEventSequence}`, {
          method: 'GET',
          headers: { accept: 'application/json' },
        });
        if (response.ok) {
          const payload = (await response.json()) as WebEventPollResponse;
          for (const event of payload.events ?? []) {
            if (Number.isFinite(event.sequence)) {
              lastEventSequence = Math.max(lastEventSequence, event.sequence);
            }
            dispatchWebEvent(event);
          }
          if ((payload.events ?? []).length === 0 && typeof payload.latestSequence === 'number') {
            lastEventSequence = Math.max(lastEventSequence, payload.latestSequence);
          }
        } else {
          await new Promise((resolve) => window.setTimeout(resolve, 1000));
        }
      } catch {
        await new Promise((resolve) => window.setTimeout(resolve, 1000));
      }
    }
  };

  const ensureEventPump = () => {
    if (eventPumpStarted) return;
    eventPumpStarted = true;
    void pollEvents();
  };

  const invoke = async (cmd: string, args: Record<string, unknown> = {}) => {
    switch (cmd) {
      case 'plugin:event|listen': {
        const event = String(args.event ?? '');
        const handlerId = Number(args.handler ?? 0);
        const eventId = Number(await callBridge(cmd, args));
        eventListeners.set(eventId, { event, handlerId });
        ensureEventPump();
        return eventId;
      }
      case 'plugin:event|unlisten': {
        const eventId = Number(args.eventId ?? 0);
        eventListeners.delete(eventId);
        return callBridge(cmd, args);
      }
      case 'plugin:event|emit':
      case 'plugin:event|emit_to':
        return callBridge(cmd, args);
      case 'plugin:window|start_dragging':
      case 'plugin:window|set_theme':
      case 'plugin:webview|set_webview_zoom':
      case 'plugin:webview|set_zoom':
        return null;
      case 'plugin:window|get_all_windows':
        return [{ label: 'main' }];
      case 'plugin:webview|get_all_webviews':
        return [{ label: 'main', windowLabel: 'main' }];
      case 'plugin:dialog|open':
      case 'plugin:dialog|save':
        return window.prompt('请输入本机文件路径，留空取消：') || null;
      case 'plugin:dialog|message': {
        const labels = getDialogButtonLabels(args.buttons);
        const message = formatDialogMessage(args);
        if (labels.confirm) {
          return window.confirm(message) ? labels.ok : labels.cancel;
        }
        window.alert(message);
        return labels.ok;
      }
      case 'plugin:dialog|ask':
      case 'plugin:dialog|confirm':
        return window.confirm(formatDialogMessage(args));
      case 'plugin:opener|open_url':
      case 'plugin:opener|openUrl': {
        const target = String(args.url ?? args.path ?? '');
        if (target) {
          window.open(target, '_blank', 'noopener,noreferrer');
        }
        return null;
      }
      case 'plugin:opener|open_path':
      case 'plugin:opener|openPath':
        return null;
      case 'plugin:updater|check':
        return null;
      case 'plugin:updater|download':
      case 'plugin:updater|install':
      case 'plugin:updater|download_and_install':
        throw new Error('Updater actions are only available in the desktop app.');
      case 'plugin:process|restart':
      case 'plugin:process|relaunch':
        throw new Error('This action is only available in the desktop app.');
      default:
        return callBridge(cmd, args);
    }
  };

  bridgeAnyWindow.__TAURI_INTERNALS__ = {
    metadata: {
      currentWindow: { label: 'main' },
      currentWebview: { label: 'main' },
    },
    invoke,
    transformCallback(callback, once = false) {
      const id = nextCallbackId++;
      callbacks.set(id, { callback, once });
      return id;
    },
    runCallback(id, ...args) {
      const entry = callbacks.get(id);
      if (!entry) return;
      entry.callback(...args);
      if (entry.once) {
        callbacks.delete(id);
      }
    },
    unregisterCallback(id) {
      callbacks.delete(id);
    },
  };

  bridgeAnyWindow.__TAURI_EVENT_PLUGIN_INTERNALS__ = {
    unregisterListener(_event, eventId) {
      eventListeners.delete(eventId);
    },
  };
}
