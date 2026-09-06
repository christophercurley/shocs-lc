const status = document.querySelector("#modes-status");
const toast = document.querySelector("#toast");
const grid = document.querySelector("#timer-grid");
const createForm = document.querySelector("#create-timer-form");
const targetTypeInput = document.querySelector("#new-timer-target-type");
const targetInput = document.querySelector("#new-timer-target");
const onInput = document.querySelector("#new-timer-on");
const offInput = document.querySelector("#new-timer-off");
const enabledInput = document.querySelector("#new-timer-enabled");
const testFacts = document.querySelector("#test-mode-facts");
const timezoneCopy = document.querySelector("#timer-timezone");
let toastTimer;
let modesData = { timezone: "", test_mode: {}, targets: [], timers: [] };

function showToast(message, isError = false) {
  clearTimeout(toastTimer);
  toast.textContent = message;
  toast.classList.toggle("is-error", isError);
  toast.classList.add("is-visible");
  toastTimer = setTimeout(() => toast.classList.remove("is-visible"), 3200);
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
      // Keep HTTP status fallback.
    }
    throw new Error(message);
  }

  return response;
}

function targetsFor(type) {
  return modesData.targets.filter((target) => target.target_type === type);
}

function populateTargetSelect(select, type, selected = null) {
  const targets = targetsFor(type);
  select.replaceChildren();

  for (const target of targets) {
    const option = document.createElement("option");
    option.value = target.target_id;
    option.textContent = target.target_type === "group"
      ? `${target.display_name} · ${target.member_count} member${target.member_count === 1 ? "" : "s"}`
      : target.display_name;
    if (selected !== null && String(target.target_id) === String(selected)) {
      option.selected = true;
    }
    select.append(option);
  }

  select.disabled = targets.length === 0;
}

function renderTestFacts() {
  const test = modesData.test_mode;
  const values = [
    ["Power ON", test.on_time ?? "—"],
    ["Power OFF", test.off_time ?? "—"],
    ["Heartbeat", test.color_interval_seconds ? `${test.color_interval_seconds}s` : "—"],
    ["Transition", test.transition_seconds !== undefined ? `${test.transition_seconds}s` : "—"],
  ];
  testFacts.replaceChildren();
  for (const [label, value] of values) {
    const row = document.createElement("div");
    const dt = document.createElement("dt");
    const dd = document.createElement("dd");
    dt.textContent = label;
    dd.textContent = value;
    row.append(dt, dd);
    testFacts.append(row);
  }
  timezoneCopy.textContent = `Timezone: ${modesData.timezone}`;
}

function makeTimerCard(timer) {
  const card = document.createElement("article");
  card.className = "timer-card";

  const heading = document.createElement("div");
  heading.className = "mode-card-heading";
  const titleBlock = document.createElement("div");
  const eyebrow = document.createElement("p");
  eyebrow.className = "eyebrow";
  eyebrow.textContent = timer.target_type.toUpperCase();
  const title = document.createElement("h2");
  title.textContent = timer.target_name;
  titleBlock.append(eyebrow, title);
  const state = document.createElement("span");
  state.className = "mode-chip";
  state.textContent = timer.enabled ? `timer · ${timer.scheduled_power}` : "disabled";
  heading.append(titleBlock, state);

  const summary = document.createElement("p");
  summary.className = "group-summary";
  summary.textContent = `${timer.member_count} member${timer.member_count === 1 ? "" : "s"} · ${timer.online_count} online · mode ${timer.mode_state ?? "none"}`;

  const form = document.createElement("div");
  form.className = "timer-edit-grid";

  const typeField = document.createElement("label");
  typeField.className = "timer-field";
  typeField.innerHTML = "<span>Target type</span>";
  const typeSelect = document.createElement("select");
  typeSelect.className = "text-input";
  for (const [value, label] of [["group", "Group"], ["light", "Light"]]) {
    const option = document.createElement("option");
    option.value = value;
    option.textContent = label;
    option.selected = timer.target_type === value;
    typeSelect.append(option);
  }
  typeField.append(typeSelect);

  const targetField = document.createElement("label");
  targetField.className = "timer-field timer-target-field";
  targetField.innerHTML = "<span>Target</span>";
  const targetSelect = document.createElement("select");
  targetSelect.className = "text-input";
  populateTargetSelect(targetSelect, timer.target_type, timer.target_id);
  targetField.append(targetSelect);

  const onField = document.createElement("label");
  onField.className = "timer-field";
  onField.innerHTML = "<span>ON</span>";
  const on = document.createElement("input");
  on.className = "text-input";
  on.type = "time";
  on.value = timer.on_time;
  onField.append(on);

  const offField = document.createElement("label");
  offField.className = "timer-field";
  offField.innerHTML = "<span>OFF</span>";
  const off = document.createElement("input");
  off.className = "text-input";
  off.type = "time";
  off.value = timer.off_time;
  offField.append(off);

  const enabledField = document.createElement("label");
  enabledField.className = "timer-enabled-field";
  const enabled = document.createElement("input");
  enabled.type = "checkbox";
  enabled.checked = timer.enabled;
  const enabledText = document.createElement("span");
  enabledText.textContent = "Enabled";
  enabledField.append(enabled, enabledText);

  typeSelect.addEventListener("change", () => {
    populateTargetSelect(targetSelect, typeSelect.value);
  });

  form.append(typeField, targetField, onField, offField, enabledField);

  const actions = document.createElement("div");
  actions.className = "timer-actions";
  const save = document.createElement("button");
  save.type = "button";
  save.className = "save-button";
  save.textContent = "Save";
  const remove = document.createElement("button");
  remove.type = "button";
  remove.className = "group-delete-button";
  remove.textContent = "Delete";
  actions.append(save, remove);

  save.addEventListener("click", async () => {
    save.disabled = true;
    try {
      await api(`/api/modes/timers/${timer.id}`, {
        method: "PUT",
        body: JSON.stringify({
          target_type: typeSelect.value,
          target_id: targetSelect.value,
          on_time: on.value,
          off_time: off.value,
          enabled: enabled.checked,
        }),
      });
      showToast(`${timer.target_name}: Timer saved`);
      await refreshModes();
    } catch (error) {
      showToast(error.message, true);
    } finally {
      save.disabled = false;
    }
  });

  remove.addEventListener("click", async () => {
    if (!window.confirm(`Delete Timer schedule for "${timer.target_name}"?`)) return;
    remove.disabled = true;
    try {
      await api(`/api/modes/timers/${timer.id}`, { method: "DELETE" });
      showToast(`${timer.target_name}: Timer deleted`);
      await refreshModes();
    } catch (error) {
      remove.disabled = false;
      showToast(error.message, true);
    }
  });

  card.append(heading, summary, form, actions);
  return card;
}

async function refreshModes() {
  try {
    const response = await fetch("/api/modes", { cache: "no-store" });
    if (!response.ok) throw new Error(`Could not load modes (${response.status})`);
    modesData = await response.json();

    renderTestFacts();
    populateTargetSelect(targetInput, targetTypeInput.value);

    grid.replaceChildren();
    if (modesData.timers.length === 0) {
      const empty = document.createElement("div");
      empty.className = "empty-card";
      empty.textContent = "No Timer schedules yet.";
      grid.append(empty);
    } else {
      for (const timer of modesData.timers) grid.append(makeTimerCard(timer));
    }

    status.textContent = `${modesData.timers.length} Timer schedule${modesData.timers.length === 1 ? "" : "s"} · ${modesData.timezone}`;
  } catch (error) {
    status.textContent = "Controller unavailable";
    showToast(error.message, true);
  }
}

targetTypeInput.addEventListener("change", () => {
  populateTargetSelect(targetInput, targetTypeInput.value);
});

createForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  const button = createForm.querySelector("button[type='submit']");
  button.disabled = true;
  try {
    if (!targetInput.value) throw new Error("No target is available for this Timer.");
    await api("/api/modes/timers", {
      method: "POST",
      body: JSON.stringify({
        target_type: targetTypeInput.value,
        target_id: targetInput.value,
        on_time: onInput.value,
        off_time: offInput.value,
        enabled: enabledInput.checked,
      }),
    });
    showToast("Timer schedule created");
    await refreshModes();
  } catch (error) {
    showToast(error.message, true);
  } finally {
    button.disabled = false;
  }
});

refreshModes();
