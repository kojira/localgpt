const API = '/api';
let sessionId = null;
let isStreaming = false;

// Initialize on DOM load
document.addEventListener('DOMContentLoaded', () => {
    loadSessions();
    setupEventListeners();
    showEmptyState();
});

function setupEventListeners() {
    document.getElementById('send').onclick = sendMessage;
    document.getElementById('new-session').onclick = newSession;

    const input = document.getElementById('input');
    input.onkeydown = (e) => {
        if (e.key === 'Enter' && !e.shiftKey) {
            e.preventDefault();
            sendMessage();
        }
    };

    // Auto-resize textarea
    input.oninput = () => {
        input.style.height = 'auto';
        input.style.height = Math.min(input.scrollHeight, 200) + 'px';
    };

    document.getElementById('session-select').onchange = (e) => {
        if (e.target.value) {
            sessionId = e.target.value;
            clearMessages();
            showEmptyState();
        }
    };
}

function showEmptyState() {
    const messages = document.getElementById('messages');
    if (messages.children.length === 0) {
        messages.innerHTML = `
            <div class="empty-state">
                <h2>Welcome to LocalGPT</h2>
                <p>Start a conversation by typing a message below.</p>
            </div>
        `;
    }
}

function clearEmptyState() {
    const emptyState = document.querySelector('.empty-state');
    if (emptyState) {
        emptyState.remove();
    }
}

async function loadSessions() {
    try {
        const res = await fetch(`${API}/sessions`);
        const data = await res.json();
        const sessions = data.sessions || [];

        const select = document.getElementById('session-select');
        if (sessions.length === 0) {
            select.innerHTML = '<option value="">No sessions</option>';
        } else {
            select.innerHTML = sessions.map(s =>
                `<option value="${s.session_id}">${s.session_id.slice(0, 8)}... (idle ${formatTime(s.idle_seconds)})</option>`
            ).join('');
            sessionId = sessions[0].session_id;
        }
    } catch (err) {
        console.error('Failed to load sessions:', err);
    }
}

function formatTime(seconds) {
    if (seconds < 60) return `${seconds}s`;
    if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
    return `${Math.floor(seconds / 3600)}h`;
}

async function newSession() {
    sessionId = null;
    clearMessages();
    showEmptyState();

    // Update select
    const select = document.getElementById('session-select');
    const newOption = document.createElement('option');
    newOption.value = '';
    newOption.text = 'New session';
    newOption.selected = true;
    select.insertBefore(newOption, select.firstChild);
}

function clearMessages() {
    document.getElementById('messages').innerHTML = '';
}

async function sendMessage() {
    if (isStreaming) return;

    const input = document.getElementById('input');
    const message = input.value.trim();
    if (!message) return;

    input.value = '';
    input.style.height = 'auto';
    clearEmptyState();

    appendMessage('user', message);
    const assistantDiv = appendMessage('assistant', '');
    assistantDiv.classList.add('loading');

    const sendBtn = document.getElementById('send');
    sendBtn.disabled = true;
    isStreaming = true;

    try {
        const res = await fetch(`${API}/chat/stream`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ message, session_id: sessionId })
        });

        if (!res.ok) {
            throw new Error(`HTTP ${res.status}: ${res.statusText}`);
        }

        const reader = res.body.getReader();
        const decoder = new TextDecoder();
        let buffer = '';

        while (true) {
            const { done, value } = await reader.read();
            if (done) break;

            buffer += decoder.decode(value, { stream: true });
            const lines = buffer.split('\n');
            buffer = lines.pop() || '';

            for (const line of lines) {
                if (!line.startsWith('data: ')) continue;
                const data = line.slice(6);
                if (data === '[DONE]') continue;

                try {
                    const event = JSON.parse(data);
                    handleEvent(event, assistantDiv);
                } catch (e) {
                    // Ignore parse errors for partial data
                }
            }
        }
    } catch (err) {
        assistantDiv.classList.remove('loading');
        assistantDiv.classList.add('error');
        assistantDiv.textContent = `Error: ${err.message}`;
    } finally {
        assistantDiv.classList.remove('loading');
        sendBtn.disabled = false;
        isStreaming = false;
        scrollToBottom();
    }
}

function handleEvent(event, assistantDiv) {
    switch (event.type) {
        case 'session':
            sessionId = event.session_id;
            updateSessionSelect(sessionId);
            break;

        case 'content':
            assistantDiv.textContent += event.delta;
            scrollToBottom();
            break;

        case 'tool_start':
            const toolStartDiv = document.createElement('div');
            toolStartDiv.className = 'message tool';
            toolStartDiv.id = `tool-${event.id}`;
            toolStartDiv.innerHTML = `<span class="tool-name">[${event.name}]</span> Running...`;
            assistantDiv.after(toolStartDiv);
            scrollToBottom();
            break;

        case 'tool_end':
            const toolEl = document.getElementById(`tool-${event.id}`);
            if (toolEl) {
                const output = event.output ? event.output.slice(0, 300) : 'Done';
                toolEl.innerHTML = `<span class="tool-name">[${event.name}]</span><div class="tool-output">${escapeHtml(output)}</div>`;
            }
            scrollToBottom();
            break;

        case 'error':
            assistantDiv.classList.add('error');
            assistantDiv.textContent = `Error: ${event.message}`;
            break;

        case 'done':
            break;
    }
}

function updateSessionSelect(newSessionId) {
    const select = document.getElementById('session-select');

    // Check if this session already exists
    for (let i = 0; i < select.options.length; i++) {
        if (select.options[i].value === newSessionId) {
            select.selectedIndex = i;
            return;
        }
    }

    // Add new session to select
    const option = document.createElement('option');
    option.value = newSessionId;
    option.text = `${newSessionId.slice(0, 8)}... (new)`;
    option.selected = true;
    select.insertBefore(option, select.firstChild);

    // Remove "New session" placeholder if exists
    for (let i = 0; i < select.options.length; i++) {
        if (select.options[i].value === '') {
            select.remove(i);
            break;
        }
    }
}

function appendMessage(role, content) {
    const div = document.createElement('div');
    div.className = `message ${role}`;
    div.textContent = content;
    document.getElementById('messages').appendChild(div);
    scrollToBottom();
    return div;
}

function scrollToBottom() {
    const container = document.getElementById('chat-container');
    container.scrollTop = container.scrollHeight;
}

function escapeHtml(text) {
    const div = document.createElement('div');
    div.textContent = text;
    return div.innerHTML;
}
