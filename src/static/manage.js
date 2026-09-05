const grid = document.querySelector("#management-grid");
const status = document.querySelector("#manage-status");
const toast = document.querySelector("#toast");
let toastTimer;

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
      // Keep status fallback for non-JSON errors.
    }
    throw new Error(message);
  }

  return response;
}

function createSwitch(checked, label, onChange) {
  const wrapper = document.createElement("label");
  wrapper.className = "switch";
  wrapper.setAttribute("aria-label", label);

  const input = document.createElement("input");
  input.type = "checkbox";
  input.checked = checked;

  const track = document.createElement("span");
  track.className = "switch-track";

  input.addEventListener("change", () => onChange(input));
  wrapper.append(input, track);
  return { wrapper, input };
}

function createManagementCard(light) {
  const card = document.createElement("article");
  card.className = "management-card";

  const header = document.createElement("div");
  header.className = "management-header";

  const heading = document.createElement("div");
  const title = document.createElement("h2");
  title.textContent = light.display_name;
  const identity = document.createElement("div");
  identity.className = "light-meta";
  identity.textContent = `${light.id}${light.address ? ` · ${light.address}` : ""}`;
  heading.append(title, identity);

  const badge = document.createElement("div");
  badge.className = "online-badge";
  badge.dataset.online = String(light.online);
  badge.innerHTML = `<span class="status-dot"></span><span>${light.online ? "Online" : "Offline"}</span>`;
  header.append(heading, badge);

  const form = document.createElement("form");
  form.className = "management-form";

  const nameField = document.createElement("label");
  nameField.className = "field-block";
  const nameLabel = document.createElement("span");
  nameLabel.className = "field-label";
  nameLabel.textContent = "Friendly name";
  const nameHelp = document.createElement("span");
  nameHelp.className = "field-help";
  nameHelp.textContent = "Stored by SHOCS and mirrored to the physical LIFX label.";
  const nameInput = document.createElement("input");
  nameInput.className = "text-input";
  nameInput.type = "text";
  nameInput.maxLength = 31;
  nameInput.value = light.friendly_name ?? "";
  nameInput.placeholder = light.device_label ?? "Unnamed LIFX light";
  nameInput.autocomplete = "off";
  nameField.append(nameLabel, nameHelp, nameInput);

  const deviceLabel = document.createElement("div");
  deviceLabel.className = "device-label-readout";
  deviceLabel.innerHTML = `
    <span>Physical label</span>
    <strong>${escapeHtml(light.device_label ?? "Unknown")}</strong>
  `;

  const enabledRow = document.createElement("div");
  enabledRow.className = "control-row management-toggle-row";
  enabledRow.innerHTML = `
    <div class="control-copy">
      <span class="control-label">SHOCS control</span>
      <span class="control-help">Keep the light in inventory, but stop SHOCS control and automation when disabled.</span>
    </div>
  `;

  const enabledSwitch = createSwitch(
    light.control_enabled,
    "SHOCS control enabled",
    async (input) => {
      const desired = input.checked;
      input.disabled = true;
      try {
        const response = await api(`/api/manage/lights/${encodeURIComponent(light.id)}`, {
          method: "PUT",
          body: JSON.stringify({ control_enabled: desired }),
        });
        const result = await response.json();
        showToast(`${title.textContent}: SHOCS control ${desired ? "enabled" : "disabled"}`);
        if (result.test_state_synced === false) {
          showToast("Saved; Test Mode synchronization will retry automatically.");
        }
      } catch (error) {
        input.checked = !desired;
        showToast(error.message, true);
      } finally {
        input.disabled = false;
        await refreshManagement();
      }
    },
  );
  enabledRow.append(enabledSwitch.wrapper);

  const footer = document.createElement("div");
  footer.className = "management-footer";
  const mode = document.createElement("span");
  mode.className = "mode-chip";
  mode.textContent = `${light.mode === "test" ? "Test" : "Custom"} mode`;
  const save = document.createElement("button");
  save.className = "save-button";
  save.type = "submit";
  save.textContent = "Save name";
  footer.append(mode, save);

  form.append(nameField, deviceLabel, enabledRow, footer);
  card.append(header, form);

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    save.disabled = true;
    const trimmed = nameInput.value.trim();

    try {
      const response = await api(`/api/manage/lights/${encodeURIComponent(light.id)}`, {
        method: "PUT",
        body: JSON.stringify({ friendly_name: trimmed }),
      });
      const result = await response.json();

      if (result.label_synced === false) {
        showToast("Name saved. Physical bulb label will sync when available.");
      } else {
        showToast(`${trimmed || "Friendly name cleared"} saved`);
      }
    } catch (error) {
      showToast(error.message, true);
    } finally {
      save.disabled = false;
      await refreshManagement();
    }
  });

  return card;
}

function escapeHtml(value) {
  const span = document.createElement("span");
  span.textContent = String(value);
  return span.innerHTML;
}

async function refreshManagement() {
  try {
    const response = await fetch("/api/manage/lights", { cache: "no-store" });
    if (!response.ok) throw new Error(`Could not load management data (${response.status})`);

    const lights = await response.json();
    grid.replaceChildren();

    if (lights.length === 0) {
      const empty = document.createElement("div");
      empty.className = "empty-card";
      empty.textContent = "No configured lights yet.";
      grid.append(empty);
    } else {
      for (const light of lights) grid.append(createManagementCard(light));
    }

    status.textContent = `${lights.length} configured light${lights.length === 1 ? "" : "s"}`;
  } catch (error) {
    status.textContent = "Controller unavailable";
    showToast(error.message, true);
  }
}

refreshManagement();
