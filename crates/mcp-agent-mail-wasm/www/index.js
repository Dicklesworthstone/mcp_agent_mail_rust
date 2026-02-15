/**
 * MCP Agent Mail - Browser Dashboard
 *
 * Transport contract:
 * - Primary: WebSocket via configured URL (default `ws://127.0.0.1:8765/ws`)
 * - Fallback: HTTP polling + ingress
 *   - `GET /mail/ws-state?limit=<n>&since=<seq>`
 *   - `POST /mail/ws-input`
 */

const DEFAULT_CONFIG = {
    websocketUrl: 'ws://127.0.0.1:8765/ws',
    fontSize: 14,
    highContrast: false,
    debugOverlay: false,
};

const TRANSPORT_MODE = Object.freeze({
    IDLE: 'idle',
    WEBSOCKET: 'websocket',
    HTTP_POLL: 'http-poll',
});

const WS_CONNECT_TIMEOUT_MS = 1500;
const HTTP_POLL_INTERVAL_MS = 450;
const HTTP_POLL_LIMIT = 200;
const MAX_EVENT_LINES = 16;

function loadConfig() {
    try {
        const saved = localStorage.getItem('agentMailConfig');
        return saved ? { ...DEFAULT_CONFIG, ...JSON.parse(saved) } : DEFAULT_CONFIG;
    } catch {
        return DEFAULT_CONFIG;
    }
}

function saveConfig(updatedConfig) {
    localStorage.setItem('agentMailConfig', JSON.stringify(updatedConfig));
}

function sleep(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms));
}

function clampInt(value, min, max, fallback) {
    const parsed = Number.parseInt(value, 10);
    if (!Number.isFinite(parsed)) return fallback;
    return Math.min(max, Math.max(min, parsed));
}

function short(text, maxLen = 72) {
    const normalized = String(text ?? '');
    if (normalized.length <= maxLen) return normalized;
    return `${normalized.slice(0, Math.max(0, maxLen - 1))}\u2026`;
}

function formatNum(value) {
    const num = Number(value ?? 0);
    return Number.isFinite(num) ? num.toLocaleString('en-US') : '0';
}

function safePathJoin(prefix, suffix) {
    const cleanPrefix = prefix.replace(/\/+$/, '');
    return `${cleanPrefix}${suffix}`.replace(/\/{2,}/g, '/');
}

function deriveHttpEndpoints(websocketUrl) {
    const url = new URL(websocketUrl);
    if (url.protocol !== 'ws:' && url.protocol !== 'wss:') {
        throw new Error(`WebSocket URL must begin with ws:// or wss://: ${websocketUrl}`);
    }
    url.protocol = url.protocol === 'wss:' ? 'https:' : 'http:';
    const pathname = url.pathname.replace(/\/+$/, '');

    let prefix = pathname;
    if (pathname.endsWith('/ws')) {
        prefix = pathname.slice(0, -3);
    } else if (pathname.endsWith('/mcp')) {
        prefix = pathname.slice(0, -4);
    } else if (pathname.endsWith('/api')) {
        prefix = pathname.slice(0, -4);
    }

    const stateUrl = new URL(url.toString());
    stateUrl.pathname = safePathJoin(prefix || '', '/mail/ws-state');
    stateUrl.search = '';
    stateUrl.hash = '';

    const inputUrl = new URL(url.toString());
    inputUrl.pathname = safePathJoin(prefix || '', '/mail/ws-input');
    inputUrl.search = '';
    inputUrl.hash = '';

    return {
        stateUrl: stateUrl.toString(),
        inputUrl: inputUrl.toString(),
    };
}

let app = null;
let config = loadConfig();
let animationFrame = null;
let lastFrameTime = 0;
let frameCount = 0;
let fps = 0;

const transportState = {
    mode: TRANSPORT_MODE.IDLE,
    pollUrl: '',
    inputUrl: '',
    pollTimer: null,
    pollInFlight: false,
    sinceSeq: null,
    lastPayload: null,
};

const elements = {
    terminal: document.getElementById('terminal'),
    loadingOverlay: document.getElementById('loading-overlay'),
    errorOverlay: document.getElementById('error-overlay'),
    errorMessage: document.getElementById('error-message'),
    retryButton: document.getElementById('retry-button'),
    connectionStatus: document.getElementById('connection-status'),
    serverUrl: document.getElementById('server-url'),
    currentScreen: document.getElementById('current-screen'),
    btnConnect: document.getElementById('btn-connect'),
    btnDisconnect: document.getElementById('btn-disconnect'),
    btnFullscreen: document.getElementById('btn-fullscreen'),
    btnSettings: document.getElementById('btn-settings'),
    settingsModal: document.getElementById('settings-modal'),
    settingsForm: document.getElementById('settings-form'),
    settingsCancel: document.getElementById('settings-cancel'),
    settingWsUrl: document.getElementById('setting-ws-url'),
    settingFontSize: document.getElementById('setting-font-size'),
    settingHighContrast: document.getElementById('setting-high-contrast'),
    settingDebug: document.getElementById('setting-debug'),
    debugOverlay: document.getElementById('debug-overlay'),
    debugFps: document.getElementById('debug-fps'),
    debugLatency: document.getElementById('debug-latency'),
    debugMessages: document.getElementById('debug-messages'),
};

function resizeCanvasToContainer() {
    const container = document.getElementById('terminal-container');
    const width = Math.max(1, Math.floor(container.clientWidth));
    const height = Math.max(1, Math.floor(container.clientHeight));
    const charWidth = Math.max(1, config.fontSize * 0.6);
    const charHeight = Math.max(1, config.fontSize);
    const cols = Math.max(1, Math.floor(width / charWidth));
    const rows = Math.max(1, Math.floor(height / charHeight));

    if (elements.terminal.width !== width) {
        elements.terminal.width = width;
    }
    if (elements.terminal.height !== height) {
        elements.terminal.height = height;
    }

    const ctx = elements.terminal.getContext('2d');
    if (ctx) {
        ctx.font = `${config.fontSize}px monospace`;
        ctx.textBaseline = 'top';
    }

    return { width, height, cols, rows, charWidth, charHeight };
}

function clearHttpPollTimer() {
    if (transportState.pollTimer) {
        clearTimeout(transportState.pollTimer);
        transportState.pollTimer = null;
    }
}

function stopHttpPolling(resetPayload = false) {
    clearHttpPollTimer();
    transportState.pollInFlight = false;
    if (resetPayload) {
        transportState.lastPayload = null;
    }
}

function scheduleHttpPoll(delayMs = HTTP_POLL_INTERVAL_MS) {
    clearHttpPollTimer();
    if (transportState.mode !== TRANSPORT_MODE.HTTP_POLL) return;
    transportState.pollTimer = setTimeout(() => {
        void pollHttpStateOnce();
    }, delayMs);
}

async function safeReadText(response) {
    try {
        return await response.text();
    } catch {
        return '';
    }
}

function extractSequence(payload) {
    if (!payload || typeof payload !== 'object') {
        return null;
    }

    const mode = payload.mode;
    const candidate = mode === 'snapshot' ? payload.next_seq : payload.to_seq;
    const seq = Number(candidate);
    if (!Number.isFinite(seq) || seq < 0) {
        return null;
    }
    return Math.floor(seq);
}

function updateTelemetryFromWasm() {
    if (!app || !app.is_connected) return;
    const screenTitle = app.screen_title || `Screen ${app.screen_id}`;
    elements.currentScreen.textContent = `${screenTitle} (#${app.screen_id})`;
    elements.debugMessages.textContent = String(Number(app.messages_received || 0));

    const timestampUs = Number(app.last_timestamp_us || 0);
    if (timestampUs > 0) {
        const nowUs = Date.now() * 1000;
        const latencyMs = Math.max(0, Math.round((nowUs - timestampUs) / 1000));
        elements.debugLatency.textContent = String(latencyMs);
    } else {
        elements.debugLatency.textContent = '--';
    }
}

function updateTelemetryFromPollPayload(payload) {
    if (!payload || typeof payload !== 'object') return;

    const mode = payload.mode || 'snapshot';
    const seq = extractSequence(payload);
    const ring = payload.event_ring_stats || {};
    elements.currentScreen.textContent = `HTTP poll ${mode} (seq ${seq ?? '-'}, next ${ring.next_seq ?? '-'})`;
    elements.debugMessages.textContent = String(Number(payload.event_count || 0));

    const generatedAtUs = Number(payload.generated_at_us || 0);
    if (generatedAtUs > 0) {
        const nowUs = Date.now() * 1000;
        const latencyMs = Math.max(0, Math.round((nowUs - generatedAtUs) / 1000));
        elements.debugLatency.textContent = String(latencyMs);
    } else {
        elements.debugLatency.textContent = '--';
    }
}

function eventSummary(event) {
    if (!event || typeof event !== 'object') return 'unknown event';
    const kind = String(event.kind || 'event');
    switch (kind) {
        case 'tool_call_start':
            return `tool:start ${short(event.tool_name || '?', 30)}`;
        case 'tool_call_end':
            return `tool:end ${short(event.tool_name || '?', 24)} ${event.duration_ms ?? '?'}ms q=${event.queries ?? 0}`;
        case 'message_sent':
            return `mail:sent #${event.id ?? '?'} ${short(event.subject || '', 36)}`;
        case 'message_received':
            return `mail:recv #${event.id ?? '?'} ${short(event.from || '?', 18)} ${short(event.subject || '', 30)}`;
        case 'reservation_granted':
            return `reservation:+ ${short(event.agent || '?', 18)} ${event.exclusive ? 'exclusive' : 'shared'}`;
        case 'reservation_released':
            return `reservation:- ${short(event.agent || '?', 18)}`;
        case 'agent_registered':
            return `agent:+ ${short(event.name || '?', 24)} (${short(event.program || '?', 20)})`;
        case 'http_request':
            return `http ${event.method || '?'} ${short(event.path || '?', 28)} -> ${event.status ?? '?'}`;
        case 'health_pulse':
            return `health pulse`;
        case 'server_started':
            return `server started ${short(event.endpoint || '', 34)}`;
        case 'server_shutdown':
            return 'server shutdown';
        default:
            return short(JSON.stringify(event), 96);
    }
}

function buildHttpFallbackLines(payload, cols) {
    const maxWidth = Math.max(16, cols - 1);
    const lines = [];
    lines.push(short('MCP Agent Mail - HTTP Poll Fallback Transport', maxWidth));
    lines.push(short(`ws: ${config.websocketUrl}`, maxWidth));
    lines.push('');

    if (!payload) {
        lines.push(short('Waiting for /mail/ws-state payload...', maxWidth));
        lines.push(short('Input and resize are sent to /mail/ws-input when available.', maxWidth));
        return lines;
    }

    const counters = payload.request_counters || {};
    const dbStats = payload.db_stats || {};
    const ring = payload.event_ring_stats || {};
    const sparkline = Array.isArray(payload.sparkline_ms)
        ? payload.sparkline_ms.slice(-8).map((v) => Math.round(Number(v) || 0)).join(', ')
        : '';

    lines.push(short(`schema=${payload.schema_version || '?'} mode=${payload.mode || '?'} transport=${payload.transport || '?'}`, maxWidth));
    lines.push(short(`event_count=${payload.event_count ?? 0} seq=${extractSequence(payload) ?? '-'} ring_next=${ring.next_seq ?? '-'}`, maxWidth));
    lines.push(short(`requests total=${formatNum(counters.total)} 2xx=${formatNum(counters.status_2xx)} 4xx=${formatNum(counters.status_4xx)} 5xx=${formatNum(counters.status_5xx)} avg=${formatNum(counters.avg_latency_ms)}ms`, maxWidth));
    lines.push(short(`db projects=${formatNum(dbStats.projects)} agents=${formatNum(dbStats.agents)} messages=${formatNum(dbStats.messages)} reservations=${formatNum(dbStats.file_reservations)} ack=${formatNum(dbStats.ack_pending)}`, maxWidth));
    lines.push(short(`sparkline(ms): ${sparkline || 'n/a'}`, maxWidth));
    lines.push('');
    lines.push('recent events:');

    const events = Array.isArray(payload.events) ? payload.events.slice(-MAX_EVENT_LINES) : [];
    if (events.length === 0) {
        lines.push('  (no events yet)');
    } else {
        for (const event of events) {
            lines.push(short(`  - ${eventSummary(event)}`, maxWidth));
        }
    }

    return lines;
}

function renderHttpFallbackFrame() {
    const ctx = elements.terminal.getContext('2d');
    if (!ctx) return;

    const metrics = resizeCanvasToContainer();
    ctx.fillStyle = config.highContrast ? '#000000' : '#1a1a2e';
    ctx.fillRect(0, 0, metrics.width, metrics.height);

    const lines = buildHttpFallbackLines(transportState.lastPayload, metrics.cols);
    const maxLines = Math.max(1, Math.floor(metrics.height / metrics.charHeight) - 1);
    ctx.fillStyle = config.highContrast ? '#ffffff' : '#dbe3ff';
    for (let i = 0; i < lines.length && i < maxLines; i += 1) {
        const y = 2 + i * metrics.charHeight;
        ctx.fillText(lines[i], 4, y);
    }
}

async function pollHttpStateOnce() {
    if (transportState.mode !== TRANSPORT_MODE.HTTP_POLL || transportState.pollInFlight) {
        return;
    }
    transportState.pollInFlight = true;
    try {
        const pollUrl = new URL(transportState.pollUrl);
        pollUrl.searchParams.set('limit', String(HTTP_POLL_LIMIT));
        if (Number.isInteger(transportState.sinceSeq) && transportState.sinceSeq >= 0) {
            pollUrl.searchParams.set('since', String(transportState.sinceSeq));
        }

        const response = await fetch(pollUrl.toString(), {
            method: 'GET',
            headers: { Accept: 'application/json' },
            cache: 'no-store',
        });
        if (!response.ok) {
            const detail = await safeReadText(response);
            throw new Error(`/mail/ws-state ${response.status} ${response.statusText}: ${short(detail, 120)}`);
        }

        const payload = await response.json();
        transportState.lastPayload = payload;
        const seq = extractSequence(payload);
        if (seq !== null) {
            transportState.sinceSeq = seq;
        }
        updateTelemetryFromPollPayload(payload);
    } catch (error) {
        console.warn('HTTP polling error:', error);
    } finally {
        transportState.pollInFlight = false;
        scheduleHttpPoll();
    }
}

async function postHttpIngress(message) {
    if (transportState.mode !== TRANSPORT_MODE.HTTP_POLL || !transportState.inputUrl) {
        return;
    }
    try {
        const response = await fetch(transportState.inputUrl, {
            method: 'POST',
            headers: {
                Accept: 'application/json',
                'Content-Type': 'application/json',
            },
            body: JSON.stringify(message),
        });
        if (!response.ok) {
            const detail = await safeReadText(response);
            throw new Error(`/mail/ws-input ${response.status}: ${short(detail, 120)}`);
        }
    } catch (error) {
        console.warn('Ingress forwarding error:', error);
    }
}

async function beginHttpPolling(reason) {
    const endpoints = deriveHttpEndpoints(config.websocketUrl);
    stopHttpPolling(true);
    if (app) {
        try {
            app.disconnect();
        } catch {
            // best-effort shutdown
        }
    }

    transportState.mode = TRANSPORT_MODE.HTTP_POLL;
    transportState.pollUrl = endpoints.stateUrl;
    transportState.inputUrl = endpoints.inputUrl;
    transportState.sinceSeq = null;

    if (reason) {
        console.warn('Switching to HTTP polling fallback:', reason);
    }

    setConnectionStatus('connected', 'Connected (HTTP poll fallback)');
    await pollHttpStateOnce();
}

async function waitForWebSocketConnection(timeoutMs) {
    const started = performance.now();
    while (performance.now() - started < timeoutMs) {
        if (app?.is_connected) return true;
        await sleep(50);
    }
    return app?.is_connected === true;
}

async function initWasm() {
    try {
        showLoading('Loading WASM module...');
        const wasm = await import('./pkg/mcp_agent_mail_wasm.js');
        await wasm.default();
        console.log('WASM module loaded, version:', wasm.version());

        app = new wasm.AgentMailApp('#terminal', config.websocketUrl);
        await app.init_canvas();
        resizeCanvasToContainer();

        hideLoading();
        updateUI();
        console.log('Agent Mail Dashboard ready');
    } catch (error) {
        console.error('Failed to initialize WASM:', error);
        showError(`Failed to load: ${error.message || error}`);
    }
}

function updateUI() {
    document.body.classList.toggle('high-contrast', Boolean(config.highContrast));
    elements.debugOverlay.classList.toggle('hidden', !config.debugOverlay);
    elements.serverUrl.textContent = config.websocketUrl;
    elements.settingWsUrl.value = config.websocketUrl;
    elements.settingFontSize.value = config.fontSize;
    elements.settingHighContrast.checked = config.highContrast;
    elements.settingDebug.checked = config.debugOverlay;
    resizeCanvasToContainer();
}

function setConnectionStatus(status, text) {
    const dot = elements.connectionStatus.querySelector('.status-dot');
    const statusText = elements.connectionStatus.querySelector('.status-text');

    dot.className = `status-dot ${status}`;
    statusText.textContent = text || status.charAt(0).toUpperCase() + status.slice(1);

    const isConnected = status === 'connected';
    elements.btnConnect.classList.toggle('hidden', isConnected);
    elements.btnDisconnect.classList.toggle('hidden', !isConnected);
}

function showLoading(message = 'Loading...') {
    elements.loadingOverlay.querySelector('p').textContent = message;
    elements.loadingOverlay.classList.remove('hidden');
    elements.errorOverlay.classList.add('hidden');
}

function hideLoading() {
    elements.loadingOverlay.classList.add('hidden');
}

function showError(message) {
    elements.errorMessage.textContent = message;
    elements.errorOverlay.classList.remove('hidden');
    elements.loadingOverlay.classList.add('hidden');
    setConnectionStatus('error', 'Error');
}

function hideError() {
    elements.errorOverlay.classList.add('hidden');
}

async function connect() {
    if (!app) return;

    hideError();
    stopHttpPolling(false);
    setConnectionStatus('connecting', 'Connecting');

    try {
        await app.connect();
        const connected = await waitForWebSocketConnection(WS_CONNECT_TIMEOUT_MS);
        if (connected) {
            transportState.mode = TRANSPORT_MODE.WEBSOCKET;
            transportState.lastPayload = null;
            setConnectionStatus('connected', 'Connected (WebSocket)');
            startRenderLoop();
            return;
        }

        await beginHttpPolling('websocket timeout');
        startRenderLoop();
    } catch (error) {
        console.warn('WebSocket connect failed; trying HTTP fallback:', error);
        try {
            await beginHttpPolling(error);
            startRenderLoop();
        } catch (fallbackError) {
            console.error('Connection failed:', fallbackError);
            showError(`Connection failed: ${fallbackError.message || fallbackError}`);
        }
    }
}

function disconnect() {
    stopHttpPolling(true);
    transportState.mode = TRANSPORT_MODE.IDLE;
    transportState.sinceSeq = null;

    if (app) {
        try {
            app.disconnect();
        } catch (error) {
            console.warn('Disconnect warning:', error);
        }
    }

    elements.currentScreen.textContent = '--';
    elements.debugLatency.textContent = '--';
    elements.debugMessages.textContent = '0';
    setConnectionStatus('disconnected', 'Disconnected');
    stopRenderLoop();
}

function startRenderLoop() {
    if (animationFrame) return;

    function render(timestamp) {
        frameCount += 1;
        if (timestamp - lastFrameTime >= 1000) {
            fps = frameCount;
            frameCount = 0;
            lastFrameTime = timestamp;
            elements.debugFps.textContent = String(fps);
        }

        if (transportState.mode === TRANSPORT_MODE.WEBSOCKET) {
            if (app) {
                try {
                    app.render();
                } catch (error) {
                    console.error('Render error:', error);
                }
            }
            updateTelemetryFromWasm();
        } else if (transportState.mode === TRANSPORT_MODE.HTTP_POLL) {
            renderHttpFallbackFrame();
            updateTelemetryFromPollPayload(transportState.lastPayload);
        }

        animationFrame = requestAnimationFrame(render);
    }

    lastFrameTime = performance.now();
    animationFrame = requestAnimationFrame(render);
}

function stopRenderLoop() {
    if (!animationFrame) return;
    cancelAnimationFrame(animationFrame);
    animationFrame = null;
}

function isTextInputFocused() {
    const focusedTag = document.activeElement?.tagName || '';
    return focusedTag === 'INPUT' || focusedTag === 'TEXTAREA' || focusedTag === 'SELECT';
}

function mapKey(event) {
    const keyMap = {
        ArrowUp: 'Up',
        ArrowDown: 'Down',
        ArrowLeft: 'Left',
        ArrowRight: 'Right',
        Escape: 'Esc',
        ' ': 'Space',
    };
    return keyMap[event.key] || event.key;
}

function keyModifiers(event) {
    let modifiers = 0;
    if (event.ctrlKey) modifiers |= 1;
    if (event.shiftKey) modifiers |= 2;
    if (event.altKey) modifiers |= 4;
    if (event.metaKey) modifiers |= 8;
    return modifiers;
}

function handleKeyDown(event) {
    if (!app || isTextInputFocused()) return;
    if (transportState.mode !== TRANSPORT_MODE.WEBSOCKET && transportState.mode !== TRANSPORT_MODE.HTTP_POLL) {
        return;
    }

    const key = mapKey(event);
    const modifiers = keyModifiers(event);

    try {
        if (transportState.mode === TRANSPORT_MODE.WEBSOCKET && app.is_connected) {
            app.send_input(key, modifiers);
        } else if (transportState.mode === TRANSPORT_MODE.HTTP_POLL) {
            void postHttpIngress({
                type: 'Input',
                data: {
                    kind: 'Key',
                    key,
                    modifiers,
                },
            });
        }
        event.preventDefault();
    } catch (error) {
        console.error('Input forwarding error:', error);
    }
}

function handleCanvasClick(event) {
    elements.terminal.focus();
    if (transportState.mode === TRANSPORT_MODE.IDLE) return;

    const rect = elements.terminal.getBoundingClientRect();
    const x = Math.floor((event.clientX - rect.left) / Math.max(1, config.fontSize * 0.6));
    const y = Math.floor((event.clientY - rect.top) / Math.max(1, config.fontSize));
    console.log('Canvas click at cell:', x, y);
}

function handleResize() {
    if (!app) return;

    const { cols, rows } = resizeCanvasToContainer();
    if (transportState.mode === TRANSPORT_MODE.WEBSOCKET && app.is_connected) {
        try {
            app.send_resize(cols, rows);
        } catch (error) {
            console.error('Resize forwarding error (websocket):', error);
        }
    } else if (transportState.mode === TRANSPORT_MODE.HTTP_POLL) {
        void postHttpIngress({
            type: 'Resize',
            data: { cols, rows },
        });
    }
}

function toggleFullscreen() {
    if (document.fullscreenElement) {
        void document.exitFullscreen();
    } else {
        void document.body.requestFullscreen();
    }
}

function openSettings() {
    elements.settingsModal.showModal();
}

function closeSettings() {
    elements.settingsModal.close();
}

function saveSettings(event) {
    event.preventDefault();
    config.websocketUrl = elements.settingWsUrl.value || DEFAULT_CONFIG.websocketUrl;
    config.fontSize = clampInt(elements.settingFontSize.value, 8, 32, DEFAULT_CONFIG.fontSize);
    config.highContrast = elements.settingHighContrast.checked;
    config.debugOverlay = elements.settingDebug.checked;

    saveConfig(config);
    updateUI();
    closeSettings();

    if (transportState.mode !== TRANSPORT_MODE.IDLE) {
        disconnect();
        setTimeout(() => {
            void connect();
        }, 300);
    }
}

function setupEventListeners() {
    elements.btnConnect.addEventListener('click', () => {
        void connect();
    });
    elements.btnDisconnect.addEventListener('click', disconnect);
    elements.retryButton.addEventListener('click', () => {
        hideError();
        void connect();
    });

    elements.btnFullscreen.addEventListener('click', toggleFullscreen);
    elements.btnSettings.addEventListener('click', openSettings);
    elements.settingsCancel.addEventListener('click', closeSettings);
    elements.settingsForm.addEventListener('submit', saveSettings);

    elements.settingsModal.addEventListener('click', (event) => {
        if (event.target === elements.settingsModal) {
            closeSettings();
        }
    });

    document.addEventListener('keydown', handleKeyDown);
    elements.terminal.addEventListener('click', handleCanvasClick);
    window.addEventListener('resize', handleResize);
    elements.terminal.focus();
}

async function main() {
    console.log('MCP Agent Mail Dashboard starting...');
    setupEventListeners();
    updateUI();
    await initWasm();
}

main().catch((error) => {
    console.error('Dashboard bootstrap failed:', error);
});
