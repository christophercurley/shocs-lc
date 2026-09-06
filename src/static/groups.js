const grid = document.querySelector("#group-grid");
const status = document.querySelector("#groups-status");
const toast = document.querySelector("#toast");
const createForm = document.querySelector("#create-group-form");
const createInput = document.querySelector("#new-group-name");
let toastTimer;
let groupData = { groups: [], lights: [] };

function showToast(message, isError = false) {
  clearTimeout(toastTimer);
  toast.textContent = message;
  toast.classList.toggle("is-error", isError);
  toast.classList.add("is-visible");
  toastTimer = setTimeout(() => toast.classList.remove("is-visible"), 3000);
}

async function api(url, options = {}) {
  const response = await fetch(url, {
    ...options,
    headers: {
      "Content-Type": "application/json",
      ...(options.headers || {}),
    },
  });

  if (!response.ok) {
    let message = `Request failed (${response.status})`;
    try {
      const body = await response.json();
      if (body.error) message = body.error;
    } catch (_) {
      // Keep the status fallback for non-JSON responses.
    }
    throw new Error(message);
  }

  return response;
}

function utf8Length(value) {
  return new TextEncoder().encode(value).length;
}

function normalizedGroupName(value) {
  return value.trim();
}

function groupNameConflict(id, candidate) {
  const key = normalizedGroupName(candidate).toLocaleLowerCase();
  return groupData.groups.find(
    (group) => group.id !== id && group.name.toLocaleLowerCase() === key,
  ) ?? null;
}

function validateGroupName(value, currentId = null) {
  const name = normalizedGroupName(value);
  if (!name) throw new Error("Group name cannot be empty.");
  if (/[\u0000-\u001F\u007F]/u.test(name)) {
    throw new Error("Group name cannot contain control characters.");
  }
  if (utf8Length(name) > 64) {
    throw new Error("Group name must be 64 UTF-8 bytes or fewer.");
  }
  const conflict = groupNameConflict(currentId, name);
  if (conflict) throw new Error(`A group named \"${conflict.name}\" already exists.`);
  return name;
}

function createLiveControlSender(url, { onError, onCommit }) {
  const intervalMs = 100;
  let pending = null;
  let inFlight = false;
  let timer = null;
  let lastSentAt = 0;

  async function transmit() {
    if (inFlight || pending === null) return;

    const payload = pending;
    pending = null;
    inFlight = true;
    lastSentAt = performance.now();

    try {
      await api(url, {
        method: "PUT",
        body: JSON.stringify(payload),
      });
      if (payload.final && onCommit) onCommit(payload);
    } catch (error) {
      onError(error);
    } finally {
      inFlight = false;
      if (pending !== null) schedule();
    }
  }

  function schedule() {
    if (inFlight || timer !== null || pending === null) return;
    const delay = Math.max(0, intervalMs - (performance.now() - lastSentAt));
    timer = setTimeout(() => {
      timer = null;
      transmit();
    }, delay);
  }

  return {
    update(payload) {
      pending = { ...payload, final: false };
      schedule();
    },
    commit(payload) {
      pending = { ...payload, final: true };
      if (!inFlight) {
        if (timer !== null) {
          clearTimeout(timer);
          timer = null;
        }
        transmit();
      }
    },
  };
}

function setBrightnessVisual(slider, percent) {
  const clamped = Math.max(0, Math.min(100, Number(percent)));
  slider.style.setProperty("--brightness-fill", `${clamped}%`);
}

function animateGroupBrightness(slider, valueLabel, from, to, durationMs) {
  const start = performance.now();
  const startValue = Number(from);
  const targetValue = Number(to);
  const duration = Math.max(0, Number(durationMs));

  if (duration === 0 || startValue === targetValue) {
    slider.value = String(Math.round(targetValue));
    valueLabel.textContent = `${Math.round(targetValue)}%`;
    setBrightnessVisual(slider, targetValue);
    return;
  }

  function frame(now) {
    const progress = Math.min(1, (now - start) / duration);
    const value = startValue + (targetValue - startValue) * progress;
    slider.value = String(Math.round(value));
    valueLabel.textContent = `${Math.round(value)}%`;
    setBrightnessVisual(slider, value);
    if (progress < 1) requestAnimationFrame(frame);
  }

  requestAnimationFrame(frame);
}

function kelvinToCss(kelvin) {
  const temperature = Math.max(1000, Math.min(40000, Number(kelvin || 3500))) / 100;
  let red;
  let green;
  let blue;

  if (temperature <= 66) {
    red = 255;
    green = 99.4708025861 * Math.log(temperature) - 161.1195681661;
    blue = temperature <= 19
      ? 0
      : 138.5177312231 * Math.log(temperature - 10) - 305.0447927307;
  } else {
    red = 329.698727446 * Math.pow(temperature - 60, -0.1332047592);
    green = 288.1221695283 * Math.pow(temperature - 60, -0.0755148492);
    blue = 255;
  }

  const clampByte = (value) => Math.round(Math.max(0, Math.min(255, value)));
  return `rgb(${clampByte(red)}, ${clampByte(green)}, ${clampByte(blue)})`;
}

function colorToCss(hue, saturation, kelvin) {
  if (Number(saturation) <= 1) return kelvinToCss(kelvin);
  return `hsl(${Number(hue)}deg ${Number(saturation)}% 50%)`;
}

function setColorThumb(thumb, hue, saturation) {
  const radius = Math.max(0, Math.min(100, Number(saturation))) / 2;
  const angle = (Number(hue) - 90) * Math.PI / 180;
  thumb.style.left = `${50 + Math.cos(angle) * radius}%`;
  thumb.style.top = `${50 + Math.sin(angle) * radius}%`;
}

function colorFromPointer(wheel, event) {
  const rect = wheel.getBoundingClientRect();
  const centerX = rect.left + rect.width / 2;
  const centerY = rect.top + rect.height / 2;
  const dx = event.clientX - centerX;
  const dy = event.clientY - centerY;
  const radius = rect.width / 2;
  const distance = Math.min(radius, Math.hypot(dx, dy));
  let hue = Math.atan2(dy, dx) * 180 / Math.PI + 90;
  if (hue < 0) hue += 360;

  return {
    hue: Math.round(hue) % 360,
    saturation: Math.round(distance / radius * 100),
  };
}

function makeButton(text, className = "group-action-button") {
  const button = document.createElement("button");
  button.type = "button";
  button.className = className;
  button.textContent = text;
  return button;
}

function createGroupCard(group) {
  const card = document.createElement("article");
  card.className = "group-card";

  const header = document.createElement("div");
  header.className = "group-card-header";

  const titleBlock = document.createElement("div");
  const title = document.createElement("h2");
  title.textContent = group.name;
  const summary = document.createElement("p");
  summary.className = "group-summary";
  summary.textContent = `${group.member_count} member${group.member_count === 1 ? "" : "s"} · ${group.online_count} online · ${group.control_enabled_count} controllable`;
  titleBlock.append(title, summary);

  const deleteButton = makeButton("Delete", "group-delete-button");
  header.append(titleBlock, deleteButton);

  const nameRow = document.createElement("div");
  nameRow.className = "group-name-row";
  const nameInput = document.createElement("input");
  nameInput.className = "text-input";
  nameInput.type = "text";
  nameInput.maxLength = 64;
  nameInput.value = group.name;
  nameInput.autocomplete = "off";
  const saveName = makeButton("Save name", "save-button");
  nameRow.append(nameInput, saveName);

  const members = document.createElement("details");
  members.className = "group-members";
  const membersSummary = document.createElement("summary");
  membersSummary.textContent = `Members (${group.member_count})`;
  members.append(membersSummary);

  const memberList = document.createElement("div");
  memberList.className = "group-member-list";
  const memberSet = new Set(group.member_ids);

  for (const light of groupData.lights) {
    const label = document.createElement("label");
    label.className = "group-member-option";

    const checkbox = document.createElement("input");
    checkbox.type = "checkbox";
    checkbox.value = light.id;
    checkbox.checked = memberSet.has(light.id);

    const copy = document.createElement("span");
    const name = document.createElement("strong");
    name.textContent = light.display_name;
    const meta = document.createElement("small");
    meta.textContent = `${light.online ? "Online" : "Offline"}${light.control_enabled ? "" : " · SHOCS control disabled"}`;
    copy.append(name, meta);

    label.append(checkbox, copy);
    memberList.append(label);
  }

  const saveMembers = makeButton("Save membership", "save-button");
  saveMembers.classList.add("group-members-save");
  members.append(memberList, saveMembers);

  const controls = document.createElement("section");
  controls.className = "group-controls";
  const controlsTitle = document.createElement("div");
  controlsTitle.className = "group-controls-heading";
  controlsTitle.innerHTML = `
    <div>
      <h3>Group control</h3>
      <p>Power preserves each light's mode. Brightness and color move controlled members to Custom.</p>
    </div>
    <span class="mode-chip">${group.power_state ?? "unknown"}</span>
  `;

  const powerRow = document.createElement("div");
  powerRow.className = "group-control-row";
  powerRow.innerHTML = `<span class="control-label">Power</span>`;
  const powerActions = document.createElement("div");
  powerActions.className = "group-power-actions";
  const offButton = makeButton("Off");
  const onButton = makeButton("On");
  powerActions.append(offButton, onButton);
  powerRow.append(powerActions);

  const modeRow = document.createElement("div");
  modeRow.className = "group-control-row";
  const modeCopy = document.createElement("div");
  modeCopy.className = "control-copy";
  modeCopy.innerHTML = `
    <span class="control-label">Mode</span>
    <span class="control-help">Applies to every member, including offline lights.</span>
  `;
  const modeActions = document.createElement("div");
  modeActions.className = "group-power-actions";
  const customModeButton = makeButton("Custom");
  const testModeButton = makeButton("Test");
  const modeState = document.createElement("span");
  modeState.className = "mode-chip";
  modeState.textContent = group.mode_state ?? "none";
  modeActions.append(customModeButton, testModeButton, modeState);
  modeRow.append(modeCopy, modeActions);

  const brightnessRow = document.createElement("div");
  brightnessRow.className = "group-brightness-row";
  const brightnessHeader = document.createElement("div");
  brightnessHeader.className = "slider-label-row";
  const brightnessLabel = document.createElement("span");
  brightnessLabel.className = "control-label";
  brightnessLabel.textContent = "Brightness";
  const brightnessValue = document.createElement("strong");
  const initialBrightness = group.brightness_percent ?? 50;
  brightnessValue.textContent = `${initialBrightness}%`;
  brightnessHeader.append(brightnessLabel, brightnessValue);
  const slider = document.createElement("input");
  slider.className = "brightness-slider";
  slider.type = "range";
  slider.min = "0";
  slider.max = "100";
  slider.step = "1";
  slider.value = String(initialBrightness);
  setBrightnessVisual(slider, initialBrightness);
  brightnessRow.append(brightnessHeader, slider);

  if (group.brightness_transition?.remaining_ms > 0) {
    animateGroupBrightness(
      slider,
      brightnessValue,
      initialBrightness,
      group.brightness_transition.to_percent,
      group.brightness_transition.remaining_ms,
    );
  }

  const colorRow = document.createElement("div");
  colorRow.className = "group-color-row";
  const colorCopy = document.createElement("div");
  colorCopy.innerHTML = `<span class="control-label">Color</span><span class="control-help">Tap to open live group color control.</span>`;
  const colorButton = document.createElement("button");
  colorButton.type = "button";
  colorButton.className = "color-swatch-button";
  colorButton.setAttribute("aria-expanded", "false");
  const swatch = document.createElement("span");
  swatch.className = "color-swatch";
  colorButton.append(swatch);
  colorRow.append(colorCopy, colorButton);

  const colorPanel = document.createElement("div");
  colorPanel.className = "color-picker-panel";
  colorPanel.hidden = true;
  const wheel = document.createElement("div");
  wheel.className = "color-wheel";
  wheel.tabIndex = 0;
  const thumb = document.createElement("span");
  thumb.className = "color-thumb";
  wheel.append(thumb);
  const readout = document.createElement("div");
  readout.className = "color-readout";
  const hueReadout = document.createElement("span");
  const satReadout = document.createElement("span");
  readout.append(hueReadout, satReadout);
  colorPanel.append(wheel, readout);

  const controlsDisabled = group.online_control_enabled_count === 0;
  const modeDisabled = group.member_count === 0;
  for (const control of [offButton, onButton, slider, colorButton]) {
    control.disabled = controlsDisabled;
  }
  customModeButton.disabled = modeDisabled;
  testModeButton.disabled = modeDisabled;

  let currentHue = group.hue_degrees ?? 0;
  let currentSaturation = group.saturation_percent ?? 0;
  let currentKelvin = group.kelvin ?? 3500;
  let colorInteracting = false;

  function renderColor() {
    swatch.style.background = colorToCss(currentHue, currentSaturation, currentKelvin);
    hueReadout.innerHTML = `Hue <strong>${Math.round(currentHue)}°</strong>`;
    satReadout.innerHTML = `Saturation <strong>${Math.round(currentSaturation)}%</strong>`;
    setColorThumb(thumb, currentHue, currentSaturation);
  }
  renderColor();

  const brightnessSender = createLiveControlSender(
    `/api/groups/${group.id}/brightness`,
    {
      onError(error) { showToast(error.message, true); },
      onCommit() { showToast(`${group.name}: group brightness updated`); },
    },
  );

  slider.addEventListener("input", () => {
    const percent = Number(slider.value);
    brightnessValue.textContent = `${percent}%`;
    setBrightnessVisual(slider, percent);
    brightnessSender.update({ percent });
  });
  slider.addEventListener("change", () => {
    const percent = Number(slider.value);
    brightnessSender.commit({ percent });
  });

  const colorSender = createLiveControlSender(
    `/api/groups/${group.id}/color`,
    {
      onError(error) { showToast(error.message, true); },
      onCommit() { showToast(`${group.name}: group color updated`); },
    },
  );

  function sendColor(final = false) {
    const payload = {
      hue_degrees: Math.round(currentHue),
      saturation_percent: Math.round(currentSaturation),
    };
    if (final) colorSender.commit(payload);
    else colorSender.update(payload);
  }

  function applyPointerColor(event, final = false) {
    const color = colorFromPointer(wheel, event);
    currentHue = color.hue;
    currentSaturation = color.saturation;
    renderColor();
    sendColor(final);
  }

  colorButton.addEventListener("click", () => {
    const opening = colorPanel.hidden;
    colorPanel.hidden = !opening;
    colorButton.setAttribute("aria-expanded", String(opening));
  });
  wheel.addEventListener("pointerdown", (event) => {
    colorInteracting = true;
    wheel.setPointerCapture(event.pointerId);
    applyPointerColor(event, false);
  });
  wheel.addEventListener("pointermove", (event) => {
    if (colorInteracting) applyPointerColor(event, false);
  });
  wheel.addEventListener("pointerup", (event) => {
    if (!colorInteracting) return;
    applyPointerColor(event, true);
    colorInteracting = false;
    if (wheel.hasPointerCapture(event.pointerId)) wheel.releasePointerCapture(event.pointerId);
  });
  wheel.addEventListener("pointercancel", () => { colorInteracting = false; });

  offButton.addEventListener("click", async () => {
    offButton.disabled = true;
    try {
      await api(`/api/groups/${group.id}/power`, {
        method: "PUT",
        body: JSON.stringify({ on: false }),
      });
      showToast(`${group.name}: off`);
    } catch (error) {
      showToast(error.message, true);
    } finally {
      offButton.disabled = controlsDisabled;
    }
  });

  onButton.addEventListener("click", async () => {
    onButton.disabled = true;
    try {
      await api(`/api/groups/${group.id}/power`, {
        method: "PUT",
        body: JSON.stringify({ on: true }),
      });
      showToast(`${group.name}: on`);
    } catch (error) {
      showToast(error.message, true);
    } finally {
      onButton.disabled = controlsDisabled;
    }
  });

  async function applyGroupMode(test) {
    customModeButton.disabled = true;
    testModeButton.disabled = true;

    try {
      const response = await api(`/api/groups/${group.id}/mode`, {
        method: "PUT",
        body: JSON.stringify({ test }),
      });
      const result = await response.json();

      const pending = result.pending_sync > 0
        ? ` · ${result.pending_sync} pending/offline`
        : "";

      showToast(
        `${group.name}: ${test ? "Test" : "Custom"} mode applied to ${result.members} member${result.members === 1 ? "" : "s"}${pending}`,
      );
      await refreshGroups();
    } catch (error) {
      showToast(error.message, true);
    } finally {
      customModeButton.disabled = modeDisabled;
      testModeButton.disabled = modeDisabled;
    }
  }

  customModeButton.addEventListener("click", () => applyGroupMode(false));
  testModeButton.addEventListener("click", () => applyGroupMode(true));

  saveName.addEventListener("click", async () => {
    saveName.disabled = true;
    try {
      const name = validateGroupName(nameInput.value, group.id);
      nameInput.value = name;
      await api(`/api/groups/${group.id}`, {
        method: "PUT",
        body: JSON.stringify({ name }),
      });
      showToast(`${name}: saved`);
      await refreshGroups();
    } catch (error) {
      showToast(error.message, true);
    } finally {
      saveName.disabled = false;
    }
  });

  saveMembers.addEventListener("click", async () => {
    saveMembers.disabled = true;
    try {
      const memberIds = [...memberList.querySelectorAll('input[type="checkbox"]:checked')]
        .map((input) => input.value);
      await api(`/api/groups/${group.id}`, {
        method: "PUT",
        body: JSON.stringify({ member_ids: memberIds }),
      });
      showToast(`${group.name}: membership saved`);
      await refreshGroups();
    } catch (error) {
      showToast(error.message, true);
    } finally {
      saveMembers.disabled = false;
    }
  });

  deleteButton.addEventListener("click", async () => {
    if (!window.confirm(`Delete group \"${group.name}\"? Lights will not be deleted.`)) return;
    deleteButton.disabled = true;
    try {
      await api(`/api/groups/${group.id}`, { method: "DELETE" });
      showToast(`${group.name}: deleted`);
      await refreshGroups();
    } catch (error) {
      deleteButton.disabled = false;
      showToast(error.message, true);
    }
  });

  controls.append(controlsTitle, powerRow, modeRow, brightnessRow, colorRow, colorPanel);
  card.append(header, nameRow, members, controls);
  return card;
}

async function refreshGroups() {
  try {
    const response = await fetch("/api/groups", { cache: "no-store" });
    if (!response.ok) throw new Error(`Could not load groups (${response.status})`);
    groupData = await response.json();

    grid.replaceChildren();
    if (groupData.groups.length === 0) {
      const empty = document.createElement("div");
      empty.className = "empty-card";
      empty.textContent = "No groups yet. Create one above.";
      grid.append(empty);
    } else {
      for (const group of groupData.groups) grid.append(createGroupCard(group));
    }

    status.textContent = `${groupData.groups.length} group${groupData.groups.length === 1 ? "" : "s"} · ${groupData.lights.length} configured lights`;
  } catch (error) {
    status.textContent = "Controller unavailable";
    showToast(error.message, true);
  }
}

createForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  const button = createForm.querySelector("button");
  button.disabled = true;
  try {
    const name = validateGroupName(createInput.value);
    await api("/api/groups", {
      method: "POST",
      body: JSON.stringify({ name }),
    });
    createInput.value = "";
    showToast(`${name}: group created`);
    await refreshGroups();
  } catch (error) {
    showToast(error.message, true);
  } finally {
    button.disabled = false;
  }
});

refreshGroups();
