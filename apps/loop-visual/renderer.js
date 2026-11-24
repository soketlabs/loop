"use strict";

const api = window.loopVisual;
const form = document.querySelector("#prompt-form");
const question = document.querySelector("#question");
const submitButton = document.querySelector("#submit-button");
const stopButton = document.querySelector("#stop-button");
const closeButton = document.querySelector("#close-visual");
const permissionsButton = document.querySelector("#permissions-button");
const statusText = document.querySelector("#status-text");
const statusMark = document.querySelector("#status-mark");
const backend = document.querySelector("#backend");
const responseState = document.querySelector("#response-state");
const output = document.querySelector("#response-output");

let activeRequestId = null;
let hasResponse = false;

function setStatus(message, state = "ready", backendName = "") {
  statusText.textContent = message;
  statusMark.dataset.state = state;
  backend.textContent = backendName;
}

function setActive(active) {
  question.disabled = active;
  submitButton.disabled = active;
  stopButton.disabled = !active;
  responseState.textContent = active ? "Working…" : "Ready when you are";
}

function beginOutput() {
  output.replaceChildren();
  const paragraph = document.createElement("p");
  paragraph.className = "answer-copy";
  output.append(paragraph);
  hasResponse = true;
  return paragraph;
}

let answerNode = null;

function appendText(text) {
  if (!answerNode) answerNode = beginOutput();
  answerNode.textContent += text;
}

function showMessage(message, kind = "error") {
  output.replaceChildren();
  const paragraph = document.createElement("p");
  paragraph.className = kind === "error" ? "message-copy error-copy" : "message-copy";
  paragraph.textContent = message;
  output.append(paragraph);
  hasResponse = true;
  answerNode = null;
}

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  const text = question.value.trim();
  if (!text || activeRequestId !== null || !api) return;

  answerNode = null;
  hasResponse = false;
  output.replaceChildren();
  activeRequestId = "pending";
  setActive(true);
  responseState.textContent = "Starting…";
  setStatus("Sending question to Loop…", "working");
  try {
    await api.prompt(text);
  } catch (error) {
    activeRequestId = null;
    setActive(false);
    showMessage(error instanceof Error ? error.message : "The request could not be started.");
    responseState.textContent = "Could not start";
    setStatus("Request failed", "error");
  }
});

stopButton.addEventListener("click", async () => {
  if (activeRequestId === null || !api) return;
  stopButton.disabled = true;
  try {
    await api.cancel();
  } catch (error) {
    showMessage(error instanceof Error ? error.message : "Could not stop the request.");
  }
});

closeButton.addEventListener("click", () => api?.closeVisual());
permissionsButton.addEventListener("click", async () => {
  permissionsButton.disabled = true;
  try {
    await api?.setupPermissions();
  } catch (error) {
    showMessage(error instanceof Error ? error.message : "Could not open permission settings.");
  } finally {
    permissionsButton.disabled = false;
  }
});

if (api) {
  api.onEvent((event) => {
    if (!event || typeof event !== "object") return;
    switch (event.type) {
      case "ready":
        setStatus(event.model ? `Ready · ${event.model}` : "Ready", "ready");
        break;
      case "status":
        setStatus(event.message || "Loop status updated", event.state || "ready", event.backend || "");
        break;
      case "started":
        activeRequestId = event.requestId;
        answerNode = null;
        output.replaceChildren();
        responseState.textContent = "Reading the screen…";
        setActive(true);
        break;
      case "text":
        if (activeRequestId === "pending" || event.requestId === activeRequestId) appendText(event.text || "");
        break;
      case "visual":
        if (event.requestId === activeRequestId) responseState.textContent = event.title || "Screen context captured";
        break;
      case "done":
        if (event.requestId !== activeRequestId) break;
        activeRequestId = null;
        setActive(false);
        responseState.textContent = "Complete";
        if (!hasResponse) showMessage("Loop finished without a text response.", "info");
        break;
      case "error":
        if (event.requestId && event.requestId !== activeRequestId) break;
        activeRequestId = null;
        setActive(false);
        responseState.textContent = "Could not complete";
        setStatus("Request failed", "error");
        showMessage(event.message || "Something went wrong. Try again.");
        break;
      case "cancelled":
        activeRequestId = null;
        setActive(false);
        responseState.textContent = "Stopped";
        setStatus("Request stopped", "ready");
        if (!hasResponse) showMessage("Request stopped.", "info");
        break;
    }
  });
} else {
  setStatus("Loop connection unavailable", "error");
  responseState.textContent = "Unavailable";
  question.disabled = true;
  submitButton.disabled = true;
  stopButton.disabled = true;
}
