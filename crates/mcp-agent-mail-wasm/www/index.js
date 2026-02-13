/**
 * MCP Agent Mail - Browser Dashboard
 *
 * This module loads the WASM terminal application and handles:
 * - WASM module initialization
 * - WebSocket connection management
 * - Keyboard and mouse input forwarding
 * - UI state management (settings, fullscreen, etc.)
 */

// Configuration defaults
const DEFAULT_CONFIG = {
    websocketUrl: 'ws://127.0.0.1:8765/ws',
    fontSize: 14,
    highContrast: false,
    debugOverlay: false,
};

// Load config from localStorage
function loadConfig() {
    try {
        const saved = localStorage.getItem('agentMailConfig');
        return saved ? { ...DEFAULT_CONFIG, ...JSON.parse(saved) } : DEFAULT_CONFIG;
    } catch {
        return DEFAULT_CONFIG;
    }
}

// Save config to localStorage
function saveConfig(config) {
    localStorage.setItem('agentMailConfig', JSON.stringify(config));
}

// Application state
let app = null;
let config = loadConfig();
let animationFrame = null;
let lastFrameTime = 0;
let frameCount = 0;
let fps = 0;

// DOM Elements
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

// Initialize WASM module
async function initWasm() {
    try {
        showLoading('Loading WASM module...');

        // Import the WASM module (built by wasm-pack)
        // The path assumes wasm-pack output with --target web
        const wasm = await import('./pkg/mcp_agent_mail_wasm.js');
        await wasm.default();

        console.log('WASM module loaded, version:', wasm.version());

        // Create application instance
        app = new wasm.AgentMailApp(
            '#terminal',
            config.websocketUrl
        );

        // Initialize canvas
        await app.init_canvas();

        hideLoading();
        updateUI();

        console.log('Agent Mail Dashboard ready');

    } catch (error) {
        console.error('Failed to initialize WASM:', error);
        showError(`Failed to load: ${error.message}`);
    }
}

// Update UI based on current state
function updateUI() {
    // Update config-based styles
    if (config.highContrast) {
        document.body.classList.add('high-contrast');
    } else {
        document.body.classList.remove('high-contrast');
    }

    // Update debug overlay visibility
    elements.debugOverlay.classList.toggle('hidden', !config.debugOverlay);

    // Update server URL display
    elements.serverUrl.textContent = config.websocketUrl;

    // Populate settings form
    elements.settingWsUrl.value = config.websocketUrl;
    elements.settingFontSize.value = config.fontSize;
    elements.settingHighContrast.checked = config.highContrast;
    elements.settingDebug.checked = config.debugOverlay;
}

// Connection status management
function setConnectionStatus(status, text) {
    const dot = elements.connectionStatus.querySelector('.status-dot');
    const statusText = elements.connectionStatus.querySelector('.status-text');

    dot.className = 'status-dot ' + status;
    statusText.textContent = text || status.charAt(0).toUpperCase() + status.slice(1);

    // Toggle connect/disconnect buttons
    const isConnected = status === 'connected';
    elements.btnConnect.classList.toggle('hidden', isConnected);
    elements.btnDisconnect.classList.toggle('hidden', !isConnected);
}

// Show loading overlay
function showLoading(message = 'Loading...') {
    elements.loadingOverlay.querySelector('p').textContent = message;
    elements.loadingOverlay.classList.remove('hidden');
    elements.errorOverlay.classList.add('hidden');
}

// Hide loading overlay
function hideLoading() {
    elements.loadingOverlay.classList.add('hidden');
}

// Show error overlay
function showError(message) {
    elements.errorMessage.textContent = message;
    elements.errorOverlay.classList.remove('hidden');
    elements.loadingOverlay.classList.add('hidden');
    setConnectionStatus('error', 'Error');
}

// Hide error overlay
function hideError() {
    elements.errorOverlay.classList.add('hidden');
}

// Connect to server
async function connect() {
    if (!app) return;

    try {
        setConnectionStatus('connecting');
        await app.connect();
        setConnectionStatus('connected');
        startRenderLoop();
    } catch (error) {
        console.error('Connection failed:', error);
        showError(`Connection failed: ${error.message || error}`);
    }
}

// Disconnect from server
function disconnect() {
    if (!app) return;

    app.disconnect();
    setConnectionStatus('disconnected');
    stopRenderLoop();
}

// Render loop
function startRenderLoop() {
    if (animationFrame) return;

    function render(timestamp) {
        // Calculate FPS
        frameCount++;
        if (timestamp - lastFrameTime >= 1000) {
            fps = frameCount;
            frameCount = 0;
            lastFrameTime = timestamp;
            elements.debugFps.textContent = fps;
        }

        // Render frame
        if (app) {
            try {
                app.render();
            } catch (e) {
                console.error('Render error:', e);
            }
        }

        // Update screen/telemetry from state-sync metadata
        if (app && app.is_connected) {
            const screenTitle = app.screen_title || `Screen ${app.screen_id}`;
            elements.currentScreen.textContent = `${screenTitle} (#${app.screen_id})`;

            const messageCount = Number(app.messages_received || 0);
            elements.debugMessages.textContent = messageCount;

            const timestampUs = Number(app.last_timestamp_us || 0);
            if (timestampUs > 0) {
                const nowUs = Date.now() * 1000;
                const latencyMs = Math.max(0, Math.round((nowUs - timestampUs) / 1000));
                elements.debugLatency.textContent = latencyMs;
            } else {
                elements.debugLatency.textContent = '-';
            }
        }

        animationFrame = requestAnimationFrame(render);
    }

    lastFrameTime = performance.now();
    animationFrame = requestAnimationFrame(render);
}

function stopRenderLoop() {
    if (animationFrame) {
        cancelAnimationFrame(animationFrame);
        animationFrame = null;
    }
}

// Keyboard event handling
function handleKeyDown(event) {
    if (!app || !app.is_connected) return;

    // Don't capture if focused on input element
    if (document.activeElement.tagName === 'INPUT') return;

    // Build modifiers byte
    let modifiers = 0;
    if (event.ctrlKey) modifiers |= 1;
    if (event.shiftKey) modifiers |= 2;
    if (event.altKey) modifiers |= 4;
    if (event.metaKey) modifiers |= 8;

    // Map special keys
    const keyMap = {
        'ArrowUp': 'Up',
        'ArrowDown': 'Down',
        'ArrowLeft': 'Left',
        'ArrowRight': 'Right',
        'Escape': 'Esc',
        ' ': 'Space',
    };

    const key = keyMap[event.key] || event.key;

    try {
        app.send_input(key, modifiers);
        event.preventDefault();
    } catch (e) {
        console.error('Input error:', e);
    }
}

// Mouse event handling
function handleCanvasClick(event) {
    if (!app || !app.is_connected) return;

    const rect = elements.terminal.getBoundingClientRect();
    const x = Math.floor((event.clientX - rect.left) / (config.fontSize * 0.6));
    const y = Math.floor((event.clientY - rect.top) / config.fontSize);

    // Focus the canvas for keyboard input
    elements.terminal.focus();

    // Note: Mouse input would be sent here if the WASM API supports it
    console.log('Mouse click at cell:', x, y);
}

// Resize handling
function handleResize() {
    if (!app) return;

    const container = document.getElementById('terminal-container');
    const charWidth = config.fontSize * 0.6;
    const charHeight = config.fontSize;

    const cols = Math.floor(container.clientWidth / charWidth);
    const rows = Math.floor(container.clientHeight / charHeight);

    if (app.is_connected) {
        try {
            app.send_resize(cols, rows);
        } catch (e) {
            console.error('Resize error:', e);
        }
    }
}

// Fullscreen toggle
function toggleFullscreen() {
    if (document.fullscreenElement) {
        document.exitFullscreen();
    } else {
        document.body.requestFullscreen();
    }
}

// Settings modal
function openSettings() {
    elements.settingsModal.showModal();
}

function closeSettings() {
    elements.settingsModal.close();
}

function saveSettings(event) {
    event.preventDefault();

    config.websocketUrl = elements.settingWsUrl.value || DEFAULT_CONFIG.websocketUrl;
    config.fontSize = parseInt(elements.settingFontSize.value) || DEFAULT_CONFIG.fontSize;
    config.highContrast = elements.settingHighContrast.checked;
    config.debugOverlay = elements.settingDebug.checked;

    saveConfig(config);
    updateUI();
    closeSettings();

    // Reconnect if URL changed
    if (app && app.is_connected) {
        disconnect();
        setTimeout(connect, 500);
    }
}

// Event listeners
function setupEventListeners() {
    // Connection buttons
    elements.btnConnect.addEventListener('click', connect);
    elements.btnDisconnect.addEventListener('click', disconnect);
    elements.retryButton.addEventListener('click', () => {
        hideError();
        connect();
    });

    // Fullscreen
    elements.btnFullscreen.addEventListener('click', toggleFullscreen);

    // Settings
    elements.btnSettings.addEventListener('click', openSettings);
    elements.settingsCancel.addEventListener('click', closeSettings);
    elements.settingsForm.addEventListener('submit', saveSettings);

    // Close modal on backdrop click
    elements.settingsModal.addEventListener('click', (e) => {
        if (e.target === elements.settingsModal) {
            closeSettings();
        }
    });

    // Keyboard input
    document.addEventListener('keydown', handleKeyDown);

    // Canvas interactions
    elements.terminal.addEventListener('click', handleCanvasClick);

    // Resize handling
    window.addEventListener('resize', handleResize);

    // Focus canvas on page load
    elements.terminal.focus();
}

// Initialize application
async function main() {
    console.log('MCP Agent Mail Dashboard starting...');

    setupEventListeners();
    updateUI();

    // Initialize WASM
    await initWasm();

    // Note: Don't auto-connect, let user click Connect
    // This allows them to review/change server URL first
}

// Start the application
main().catch(console.error);
