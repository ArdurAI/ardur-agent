import { contentTextFromPayload, splitSse } from './sse.js';

const transcript = document.querySelector('#transcript');
const form = document.querySelector('#chat-form');
const messageInput = document.querySelector('#message');
const serverUrlInput = document.querySelector('#server-url');
const bearerTokenInput = document.querySelector('#bearer-token');
const adminTokenInput = document.querySelector('#admin-token');
const installButton = document.querySelector('#install-button');
const pushButton = document.querySelector('#enable-push');
const pushStatus = document.querySelector('#push-status');
const approvalPanel = document.querySelector('#approval-panel');
const approvalIdText = document.querySelector('#approval-id');
const approvalStatus = document.querySelector('#approval-status');
const approveButton = document.querySelector('#approve-button');
const rejectButton = document.querySelector('#reject-button');

let deferredInstallPrompt;

function appendMessage(role, text) {
  const item = document.createElement('li');
  item.className = `message ${role}`;
  item.textContent = text;
  transcript.append(item);
  item.scrollIntoView({ block: 'end', behavior: 'smooth' });
  return item;
}

function serverUrl(path) {
  return new URL(path, serverUrlInput.value.replace(/\/?$/, '/')).toString();
}

function bearerHeaders(tokenInput) {
  const token = tokenInput.value.trim();
  return token ? { Authorization: ['Bearer', token].join(' ') } : {};
}

function chatHeaders() {
  return bearerHeaders(bearerTokenInput);
}

function adminHeaders() {
  const token = adminTokenInput.value.trim();
  return token ? bearerHeaders(adminTokenInput) : chatHeaders();
}

async function streamChat(message) {
  appendMessage('user', message);
  const assistant = appendMessage('assistant', '');
  const response = await fetch(serverUrl('/chat'), {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Accept: 'text/event-stream',
      ...chatHeaders(),
    },
    body: JSON.stringify({ message, stream: true }),
  });

  if (!response.ok || !response.body) {
    throw new Error(`chat request failed: HTTP ${response.status}`);
  }

  const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
  let buffer = '';
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    const split = splitSse(buffer, value);
    buffer = split.buffer;
    for (const parsed of split.payloads) {
      assistant.textContent += contentTextFromPayload(parsed);
    }
  }
  if (!assistant.textContent.trim()) assistant.textContent = '[no reply text returned]';
}

form.addEventListener('submit', async (event) => {
  event.preventDefault();
  const message = messageInput.value.trim();
  if (!message) return;
  messageInput.value = '';
  try {
    await streamChat(message);
  } catch (error) {
    appendMessage('error', error instanceof Error ? error.message : String(error));
  }
});

window.addEventListener('beforeinstallprompt', (event) => {
  event.preventDefault();
  deferredInstallPrompt = event;
  installButton.disabled = false;
});

installButton.addEventListener('click', async () => {
  if (!deferredInstallPrompt) return;
  deferredInstallPrompt.prompt();
  await deferredInstallPrompt.userChoice;
  deferredInstallPrompt = undefined;
  installButton.disabled = true;
});

async function registerServiceWorker() {
  if (!('serviceWorker' in navigator)) {
    pushStatus.textContent = 'Service workers are not supported in this browser.';
    return null;
  }
  return navigator.serviceWorker.register('./sw.js');
}

async function enablePushApprovals() {
  const registration = await registerServiceWorker();
  if (!registration || !('PushManager' in window)) {
    pushStatus.textContent = 'Push notifications are not supported in this browser.';
    return;
  }
  const permission = await Notification.requestPermission();
  if (permission !== 'granted') {
    pushStatus.textContent = 'Notification permission was not granted.';
    return;
  }
  const existing = await registration.pushManager.getSubscription();
  pushStatus.textContent = existing
    ? 'Approval push subscription already exists.'
    : 'Push is permitted. Register the subscription endpoint when the server exposes VAPID keys.';
}

pushButton.addEventListener('click', () => {
  enablePushApprovals().catch((error) => {
    pushStatus.textContent = error instanceof Error ? error.message : String(error);
  });
});

function currentApprovalId() {
  return new URLSearchParams(window.location.search).get('approval_id') || approvalIdText.textContent;
}

function showApproval(id) {
  if (!id) return;
  approvalIdText.textContent = id;
  approvalPanel.hidden = false;
}

async function submitApproval(action) {
  const approvalId = currentApprovalId();
  if (!approvalId) return;
  approvalStatus.textContent = `${action} pending…`;
  const response = await fetch(serverUrl(`/approvals/${encodeURIComponent(approvalId)}/${action}`), {
    method: 'POST',
    headers: adminHeaders(),
  });
  approvalStatus.textContent = response.ok
    ? `${action} recorded.`
    : `${action} failed with HTTP ${response.status}.`;
}

approveButton.addEventListener('click', () => submitApproval('approve'));
rejectButton.addEventListener('click', () => submitApproval('reject'));

navigator.serviceWorker?.addEventListener('message', (event) => {
  if (event.data?.type !== 'approval-action') return;
  showApproval(event.data.approvalId);
  if (event.data.action === 'approve' || event.data.action === 'reject') {
    submitApproval(event.data.action).catch((error) => {
      approvalStatus.textContent = error instanceof Error ? error.message : String(error);
    });
  }
});

showApproval(currentApprovalId());
registerServiceWorker().catch(() => {});
