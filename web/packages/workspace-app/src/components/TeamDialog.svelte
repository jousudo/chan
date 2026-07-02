<script lang="ts">
  import { Bot, X } from "lucide-svelte";
  import { onMount } from "svelte";
  import { api, type TeamTemplateInfo, type SkillEntry } from "../api/client";
  import {
    clearTeamWorkPending,
    closeTab,
    setTeamWorkPendingConfig,
    teamWorkPendingConfig,
  } from "../state/tabs.svelte";
  import { scheduleSessionSave } from "../state/store.svelte";
  import {
    assignMemberToCell,
    closeTeamDialog,
    defaultTabGroupFromPath,
    defaultTeamConfig,
    gridShapesForSize,
    reshapeSplitGrid,
    resizeTeamMembers,
    autoAssignSlots,
    switchRealEstate,
    TEAM_MAX_SIZE,
    TEAM_MIN_SIZE,
    type GridShape,
    type TeamConfigMode,
    type TeamDialogConfig,
    type TeamDialogRequest,
    type TeamMemberDraft,
    unassignMember,
    validateTeamConfig,
  } from "../state/teamDialog.svelte";
  import { runTeamBootstrap, translateConfig, wireToDialog } from "../state/teamOrchestrator.svelte";
  import { TEAM_DIR_DEFAULT } from "../state/teamConfigPath";

  /// Team Work dialog. Opens over the already-created Team Work Lead
  /// terminal (the dialog request
  /// carries that tab + pane id). Cancel deletes the lead tab;
  /// Bootstrap runs the lead-first orchestrator against it.

  let {
    request,
  }: {
    request: TeamDialogRequest;
  } = $props();

  // The dialog is unmounted + remounted across requests so this
  // single-shot capture is intended. The lead terminal carries the live config
  // draft (seeded with defaultTeamConfig at Cmd+P), so seeding from it here both
  // resumes a normal open AND restores the in-progress config after a window
  // reload reopened this dialog (#4 reload-survival).
  // svelte-ignore state_referenced_locally
  let config: TeamDialogConfig = $state(
    teamWorkPendingConfig(request) ?? defaultTeamConfig(),
  );
  // Tracks the last auto-derived tab-group so editing the config path
  // keeps the group in sync UNTIL the user hand-edits it: while
  // `config.tabGroup` still equals this last auto value it is re-derived
  // from the new path; once the user types their own group it diverges
  // and stays put. (Sync logic lives in `syncTabGroupToPath`.)
  // svelte-ignore state_referenced_locally
  let lastAutoTabGroup = $state(config.tabGroup);
  let busy = $state(false);
  let submitError = $state<string | null>(null);
  // Load-mode path validation. `loadError` carries the backend 400;
  // the success surface lives in `loadedConfig` below.
  let loadError = $state<string | null>(null);
  // Surfaces the config.toml found in the loaded directory so the user
  // sees WHAT they are about to bootstrap (TW1). Cleared whenever the
  // dir input changes or the New/Load mode flips.
  let loadedConfig = $state<{ teamName: string; memberCount: number } | null>(
    null,
  );
  // Directory autocomplete for the team-dir field (TW1). Listed one
  // level at a time off the typed parent segment; files are excluded so
  // the field nudges toward a directory choice. A request id drops stale
  // responses when the user types faster than the round-trip.
  let dirSuggestions = $state<string[]>([]);
  let dirSuggestReq = 0;
  let nameInputEl = $state<HTMLInputElement | undefined>();

  onMount(() => {
    queueMicrotask(() => nameInputEl?.focus());
    void refreshDirSuggestions(config.teamDir);
  });

  // Mirror the live config draft onto the lead terminal and schedule a session
  // save on every edit, so a window reload reopens this dialog with exactly
  // what the user was editing (#4 reload-survival). The App-root layout effect
  // doesn't track `teamWorkPending`, so the save has to be scheduled here; it's
  // debounced, so a burst of keystrokes collapses to one write.
  $effect(() => {
    setTeamWorkPendingConfig(request, config);
    scheduleSessionSave();
  });

  const issue = $derived<string | null>(validateTeamConfig(config));

  // Info line for New mode: the workspace-relative dir the team files
  // land in (the value the user typed, trailing slash trimmed).
  const teamDir = $derived(config.teamDir.replace(/\/+$/, ""));

  /// Keep the tab-group name following the team-dir basename as the
  /// user edits the dir, but only while they have not hand-edited the
  /// group (tracked via `lastAutoTabGroup`). Called on every team-dir
  /// input.
  function syncTabGroupToPath(): void {
    const prevAuto = lastAutoTabGroup;
    const nextAuto = defaultTabGroupFromPath(config.teamDir);
    lastAutoTabGroup = nextAuto;
    if (config.tabGroup === prevAuto) config.tabGroup = nextAuto;
  }

  function setMemberField<K extends keyof TeamMemberDraft>(
    idx: number,
    field: K,
    value: TeamMemberDraft[K],
  ): void {
    config = {
      ...config,
      members: config.members.map((m, i) =>
        i === idx ? { ...m, [field]: value } : m,
      ),
    };
  }

  function setLead(idx: number): void {
    config = {
      ...config,
      members: config.members.map((m, i) => ({ ...m, isLead: i === idx })),
    };
  }

  function onSizeChange(next: number): void {
    const clamped = Math.max(TEAM_MIN_SIZE, Math.min(TEAM_MAX_SIZE, next));
    config = resizeTeamMembers({ ...config, size: clamped });
  }

  function setConfigMode(mode: TeamConfigMode): void {
    if (config.configMode === mode) return;
    config = { ...config, configMode: mode };
    loadError = null;
    loadedConfig = null;
    void refreshDirSuggestions(config.teamDir);
  }

  /// Populate the team-dir autocomplete from the workspace. Lists the
  /// directories directly under the typed parent segment (everything up
  /// to the last "/"), so typing "teams/" offers "teams/alpha",
  /// "teams/beta", etc. Files are filtered out: a team config always
  /// lives in a directory, so the field forces a directory choice. The
  /// request id guards against out-of-order responses.
  ///
  /// A BARE prefix (no "/") must work too: typing "foo" suggests every
  /// root dir starting with "foo" as "foo/", so loading an existing team
  /// does not require typing the trailing "/" first. We prefix-filter in
  /// JS BEFORE the cap so a match is never sliced off, and append "/" so
  /// each suggestion reads as a directory to descend into (validateAndLoad
  /// strips the trailing slash before reading {dir}/config.toml).
  async function refreshDirSuggestions(value: string): Promise<void> {
    const slash = value.lastIndexOf("/");
    const parent = slash >= 0 ? value.slice(0, slash) : "";
    const req = ++dirSuggestReq;
    try {
      const entries = await api.list(parent || null);
      if (req !== dirSuggestReq) return;
      const needle = value.toLowerCase();
      dirSuggestions = entries
        .filter((e) => e.is_dir)
        .map((e) => e.path)
        .filter((p) => p.toLowerCase().startsWith(needle))
        .map((p) => `${p}/`)
        .slice(0, 50);
    } catch {
      if (req === dirSuggestReq) dirSuggestions = [];
    }
  }

  /// Team-dir input handler: keep the tab-group following the path,
  /// refresh the directory autocomplete, and clear any stale load
  /// result so the surfaced config never lags the typed path.
  function onTeamDirInput(value: string): void {
    syncTabGroupToPath();
    void refreshDirSuggestions(value);
    loadError = null;
    loadedConfig = null;
  }

  /// Load mode: on team-dir entry, read + validate the team's
  /// config.toml via the backend. On success, prepopulate the form
  /// from the loaded config (the user is now in a pre-populated New
  /// form, still editable). On failure, surface the 400 message
  /// inline.
  async function validateAndLoad(): Promise<void> {
    loadError = null;
    loadedConfig = null;
    // Trailing slashes are a natural artifact of directory autocomplete;
    // strip them so `{dir}/config.toml` resolves and the field reads as a
    // directory path.
    const path = config.teamDir.trim().replace(/\/+$/, "");
    if (!path) {
      loadError = "Team directory required";
      return;
    }
    busy = true;
    try {
      const wire = await api.readTeamConfig(path);
      const loaded = wireToDialog(wire, path);
      config = resizeTeamMembers(loaded);
      loadedConfig = {
        teamName: wire.team_name,
        memberCount: wire.members.length,
      };
    } catch (err) {
      loadError = (err as Error).message ?? String(err);
    } finally {
      busy = false;
    }
  }

  // ---- split-pane airplane grid (drag&drop) ----------------------
  let draggingMember = $state<number | null>(null);

  function onMemberDragStart(idx: number, e: DragEvent): void {
    draggingMember = idx;
    if (e.dataTransfer) {
      e.dataTransfer.effectAllowed = "move";
      e.dataTransfer.setData("text/plain", `team-member-${idx}`);
    }
  }

  function onMemberDragEnd(): void {
    draggingMember = null;
  }

  function onCellDragOver(e: DragEvent): void {
    if (draggingMember === null) return;
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
  }

  function onCellDrop(cellIdx: number, e: DragEvent): void {
    e.preventDefault();
    if (draggingMember === null) return;
    config = assignMemberToCell(config, draggingMember, cellIdx);
    draggingMember = null;
  }

  function onUnassignClick(memberIdx: number): void {
    config = unassignMember(config, memberIdx);
  }

  function setRealEstate(kind: "tabs" | "split"): void {
    config = switchRealEstate(config, kind);
  }

  function onShapeClick(shape: GridShape): void {
    config = reshapeSplitGrid(config, shape);
  }

  /// Distribute every unassigned member across the current grid's cells
  /// (least-populated first). No-op outside split mode.
  function autoAssign(): void {
    if (config.realEstate.kind !== "split") return;
    const slots = autoAssignSlots(config.realEstate.slots, config.members.length);
    config = { ...config, realEstate: { ...config.realEstate, slots } };
  }

  function cellOfMember(memberIdx: number): number | null {
    if (config.realEstate.kind !== "split") return null;
    for (let i = 0; i < config.realEstate.slots.length; i += 1) {
      if (config.realEstate.slots[i].includes(memberIdx)) return i;
    }
    return null;
  }

  /// Render a member's handle as it'll appear in chan + the
  /// downstream agent's `CHAN_TAB_NAME`.
  function handleOf(member: TeamMemberDraft): string {
    if (!config.autoPrefix) return member.name;
    return member.name.startsWith("@@") ? member.name : `@@${member.name}`;
  }

  async function onBootstrap(): Promise<void> {
    submitError = null;
    if (issue) {
      submitError = issue;
      return;
    }
    busy = true;
    try {
      await runTeamBootstrap(config, {
        leadTabId: request.leadTabId,
        leadPaneId: request.leadPaneId,
      });
      // The lead terminal is now a committed lead, not a pending dialog: drop
      // the reopen-on-reload flag + persist so a later reload doesn't resurrect
      // this dialog over it (#4).
      clearTeamWorkPending(request);
      scheduleSessionSave();
      closeTeamDialog();
    } catch (err) {
      submitError = `bootstrap failed: ${(err as Error).message}`;
    } finally {
      busy = false;
    }
  }

  /// Cancel/dismiss: delete the exact Team Work Lead terminal tab
  /// that Cmd+P created, then dismiss the dialog (restoring the
  /// previous state).
  function onCancel(): void {
    void closeTab(request.leadPaneId, request.leadTabId, { force: true });
    closeTeamDialog();
  }

  function onBackdropClick(e: MouseEvent): void {
    if (e.target === e.currentTarget) onCancel();
  }

  // ---- Team templates --------------------------------------------------
  let templates = $state<TeamTemplateInfo[]>([]);
  let templatesOpen = $state(false);
  let templatesBusy = $state(false);
  let templateSaveName = $state("");
  let templateSaveError = $state<string | null>(null);
  let templateImportError = $state<string | null>(null);
  let templateImportInput = $state<HTMLInputElement | undefined>();

  async function refreshTemplates(): Promise<void> {
    try {
      templates = await api.listTeamTemplates();
    } catch {
      templates = [];
    }
  }

  async function applyTemplate(name: string): Promise<void> {
    templatesBusy = true;
    templateSaveError = null;
    try {
      const wire = await api.getTeamTemplate(name);
      // Keep the current team directory; the template does not carry a
      // meaningful workspace path, so the user's existing dir stays.
      const loaded = wireToDialog(wire, config.teamDir || TEAM_DIR_DEFAULT);
      config = resizeTeamMembers({ ...loaded, configMode: "new" });
      loadError = null;
      loadedConfig = null;
    } catch (err) {
      templateSaveError = `load failed: ${(err as Error).message}`;
    } finally {
      templatesBusy = false;
    }
  }

  async function saveAsTemplate(): Promise<void> {
    const name = templateSaveName.trim();
    if (!name) {
      templateSaveError = "Enter a template name";
      return;
    }
    templatesBusy = true;
    templateSaveError = null;
    try {
      const wire = translateConfig(config);
      await api.saveTeamTemplate(name, wire);
      templateSaveName = "";
      await refreshTemplates();
    } catch (err) {
      templateSaveError = (err as Error).message;
    } finally {
      templatesBusy = false;
    }
  }

  async function deleteTemplate(name: string): Promise<void> {
    templatesBusy = true;
    try {
      await api.deleteTeamTemplate(name);
      await refreshTemplates();
    } catch (err) {
      templateSaveError = `delete failed: ${(err as Error).message}`;
    } finally {
      templatesBusy = false;
    }
  }

  async function onImportFile(e: Event): Promise<void> {
    const file = (e.currentTarget as HTMLInputElement).files?.[0];
    if (!file) return;
    templatesBusy = true;
    templateImportError = null;
    try {
      await api.importTeamTemplate(file);
      await refreshTemplates();
    } catch (err) {
      templateImportError = `import failed: ${(err as Error).message}`;
    } finally {
      templatesBusy = false;
      // Reset the file input so the same file can be re-imported after
      // a name collision is resolved by the user renaming the template.
      if (templateImportInput) templateImportInput.value = "";
    }
  }

  function toggleTemplates(): void {
    templatesOpen = !templatesOpen;
    if (templatesOpen && templates.length === 0) {
      void refreshTemplates();
    }
  }

  // ---- Team skills (Engineering Standards) -------------------------
  let skillsOpen = $state(false);
  let skillAddName = $state("");
  let skillAddContent = $state("");
  let skillAddOpen = $state(false);
  let skillEditIdx = $state<number | null>(null);
  let skillEditContent = $state("");

  function addSkill(): void {
    const name = skillAddName.trim();
    if (!name) return;
    config = { ...config, skills: [...config.skills, { name, content: skillAddContent }] };
    skillAddName = "";
    skillAddContent = "";
    skillAddOpen = false;
  }

  function removeSkill(idx: number): void {
    const removed = config.skills[idx].name;
    config = {
      ...config,
      skills: config.skills.filter((_, i) => i !== idx),
      members: config.members.map((m) => ({
        ...m,
        skills: m.skills.filter((s) => s !== removed),
      })),
    };
    if (skillEditIdx === idx) skillEditIdx = null;
  }

  function startEditSkill(idx: number): void {
    skillEditIdx = idx;
    skillEditContent = config.skills[idx].content;
  }

  function saveSkillEdit(idx: number): void {
    config = {
      ...config,
      skills: config.skills.map((s, i) =>
        i === idx ? { ...s, content: skillEditContent } : s,
      ),
    };
    skillEditIdx = null;
  }

  function toggleMemberSkill(memberIdx: number, skillName: string): void {
    const member = config.members[memberIdx];
    const has = member.skills.includes(skillName);
    setMemberField(
      memberIdx,
      "skills",
      has ? member.skills.filter((s) => s !== skillName) : [...member.skills, skillName],
    );
  }

  function onKeydown(e: KeyboardEvent): void {
    if (e.key === "Escape" && !busy) {
      // Capture phase + stopPropagation: Cmd+P opens this dialog OVER a
      // freshly-spawned lead terminal whose xterm (and the Team Work
      // compose box) grabs focus, so a bubble-phase Escape was consumed
      // by the terminal before reaching the window. Listening in capture
      // runs this first, and stopPropagation keeps the keystroke out of
      // the terminal behind the modal.
      e.preventDefault();
      e.stopPropagation();
      onCancel();
    }
  }
</script>

<svelte:window onkeydowncapture={onKeydown} />

<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="team-dialog-backdrop" onclick={onBackdropClick} role="presentation">
  <div
    class="team-dialog"
    role="dialog"
    aria-modal="true"
    aria-labelledby="team-dialog-title"
  >
    <header class="team-dialog-header">
      <h2 id="team-dialog-title">Spawn agents</h2>
      <button
        type="button"
        class="team-dialog-close"
        onclick={onCancel}
        aria-label="Close"
      >
        <X size={16} strokeWidth={1.75} />
      </button>
    </header>

    <div class="team-dialog-body">
      <label class="team-field">
        <span class="team-field-label">Your name</span>
        <input
          bind:this={nameInputEl}
          bind:value={config.hostName}
          type="text"
          placeholder="Neo"
          autocomplete="off"
        />
        <span class="team-field-hint">
          Renders as <code>{handleOf({ name: config.hostName || "(name)", command: "", env: "", isLead: false, skills: [] })}</code>
          when joining the team.
        </span>
      </label>

      <label class="team-checkbox-row">
        <input type="checkbox" bind:checked={config.autoPrefix} />
        <span>Auto-prefix names with <code>@@</code></span>
      </label>

      <label class="team-checkbox-row">
        <input type="checkbox" bind:checked={config.mcpEnv} />
        <span>Expose chan MCP env vars to the team's terminals (off by default)</span>
      </label>

      <fieldset class="team-realestate">
        <legend>Team configuration</legend>
        <div
          class="team-realestate-toggle"
          role="radiogroup"
          aria-label="Team configuration source"
        >
          <button
            type="button"
            class="team-realestate-mode"
            class:on={config.configMode === "new"}
            onclick={() => setConfigMode("new")}
          >
            New
          </button>
          <button
            type="button"
            class="team-realestate-mode"
            class:on={config.configMode === "load"}
            onclick={() => setConfigMode("load")}
          >
            Load
          </button>
        </div>

        <label class="team-field">
          <span class="team-field-label">Team directory (in workspace)</span>
          <input
            bind:value={config.teamDir}
            type="text"
            placeholder="new-team-1"
            autocomplete="off"
            list="team-dir-suggestions"
            oninput={(e) =>
              onTeamDirInput((e.currentTarget as HTMLInputElement).value)}
            onchange={() => {
              if (config.configMode === "load") void validateAndLoad();
            }}
          />
          <datalist id="team-dir-suggestions">
            {#each dirSuggestions as d (d)}
              <option value={d}></option>
            {/each}
          </datalist>
          {#if config.configMode === "new"}
            <span class="team-field-hint">
              Team files will be created in <code>&lt;workspace&gt;/{teamDir}/</code>
            </span>
          {:else if loadError}
            <span class="team-field-hint team-load-error" role="alert">
              {loadError}
            </span>
          {:else if loadedConfig}
            <!-- Surface the config.toml the dir resolved to so the user
                 sees exactly what they are about to bootstrap (TW1). -->
            <span class="team-load-found" role="status">
              <code class="team-load-file"
                >{config.teamDir.replace(/\/+$/, "")}/config.toml</code
              >
              <span class="team-load-meta">
                {loadedConfig.teamName} &middot; {loadedConfig.memberCount}
                member{loadedConfig.memberCount === 1 ? "" : "s"}
              </span>
            </span>
          {:else}
            <span class="team-field-hint">
              Enter the directory of an existing team in the workspace to load it.
            </span>
          {/if}
        </label>

        <label class="team-field">
          <span class="team-field-label">Terminal tab group name</span>
          <input
            bind:value={config.tabGroup}
            type="text"
            placeholder="chan-team"
            autocomplete="off"
          />
          <span class="team-field-hint">
            Every team terminal joins this tab group. Defaults to the team
            directory name; a <code>-N</code> suffix is added at bootstrap if the
            name is already in use.
          </span>
        </label>

        <label class="team-field">
          <span class="team-field-label">Brief (optional)</span>
          <textarea
            bind:value={config.brief}
            rows="5"
            placeholder="Describe the team's workflow, approval flow, and communication hierarchy."
            autocomplete="off"
          ></textarea>
          <span class="team-field-hint">
            Team process doc folded verbatim into <code>bootstrap.md</code> (after
            the Roster). Persisted in <code>config.toml</code> and carried by
            exported templates. Leave empty for the generic bootstrap.
          </span>
        </label>
      </fieldset>

      <!-- Templates: globally saved TeamConfig snapshots in ~/.chan/team-templates/.
           Open/closed state persists for the dialog's lifetime; the list is
           fetched lazily on first open. Collapsed by default to keep the dialog
           compact for users who never use templates. -->
      <fieldset class="team-templates-fieldset">
        <legend>
          <button
            type="button"
            class="team-templates-toggle"
            onclick={toggleTemplates}
            aria-expanded={templatesOpen}
          >
            Templates
            <span class="team-templates-caret" aria-hidden="true">
              {templatesOpen ? "v" : ">"}
            </span>
          </button>
        </legend>

        {#if templatesOpen}
          <div class="team-templates-body">
            {#if templates.length === 0}
              <p class="team-templates-empty">No saved templates.</p>
            {:else}
              <ul class="team-templates-list">
                {#each templates as t (t.name)}
                  <li class="team-templates-item">
                    <span class="team-templates-name">{t.name}</span>
                    <span class="team-templates-meta">
                      {t.team_name} &middot; {t.member_count}
                      member{t.member_count === 1 ? "" : "s"}
                    </span>
                    <span class="team-templates-actions">
                      <button
                        type="button"
                        class="team-tpl-btn"
                        disabled={templatesBusy}
                        onclick={() => void applyTemplate(t.name)}
                        title="Load this template into the form"
                      >Load</button>
                      <a
                        class="team-tpl-btn"
                        href={api.teamTemplateExportUrl(t.name)}
                        download="{t.name}.toml"
                        title="Download as TOML"
                      >Export</a>
                      <button
                        type="button"
                        class="team-tpl-btn team-tpl-danger"
                        disabled={templatesBusy}
                        onclick={() => void deleteTemplate(t.name)}
                        title="Delete this template"
                      >Delete</button>
                    </span>
                  </li>
                {/each}
              </ul>
            {/if}

            <div class="team-tpl-save-row">
              <input
                type="text"
                class="team-tpl-name-input"
                bind:value={templateSaveName}
                placeholder="template-name"
                autocomplete="off"
              />
              <button
                type="button"
                class="team-tpl-btn"
                disabled={templatesBusy || !templateSaveName.trim()}
                onclick={() => void saveAsTemplate()}
              >Save as template</button>
            </div>

            <div class="team-tpl-import-row">
              <label class="team-tpl-btn team-tpl-import-label">
                Import TOML
                <input
                  bind:this={templateImportInput}
                  type="file"
                  accept=".toml"
                  class="team-tpl-file-input"
                  onchange={onImportFile}
                />
              </label>
              {#if templateImportError}
                <span class="team-tpl-error" role="alert">{templateImportError}</span>
              {/if}
            </div>

            {#if templateSaveError}
              <p class="team-tpl-error" role="alert">{templateSaveError}</p>
            {/if}
          </div>
        {/if}
      </fieldset>

      <!-- Engineering standards: named skill profiles embedded in the team config.
           Each skill applies to all members; members may also carry additional
           skills from this pool. Content is embedded inline for portability
           (no .agents/skills/ dependency in the destination workspace). -->
      <fieldset class="team-skills-fieldset">
        <legend>
          <button
            type="button"
            class="team-skills-toggle"
            onclick={() => { skillsOpen = !skillsOpen; }}
            aria-expanded={skillsOpen}
          >
            Engineering Standards
            {#if config.skills.length > 0}
              <span class="team-skills-count">({config.skills.length})</span>
            {/if}
            <span class="team-skills-caret" aria-hidden="true">
              {skillsOpen ? "v" : ">"}
            </span>
          </button>
        </legend>

        {#if skillsOpen}
          <div class="team-skills-body">
            {#if config.skills.length === 0}
              <p class="team-skills-empty">No skills defined. Add a skill to embed technical standards in this team config.</p>
            {:else}
              <ul class="team-skills-list">
                {#each config.skills as skill, idx (idx)}
                  <li class="team-skills-item">
                    <div class="team-skills-item-header">
                      <span class="team-skills-name">{skill.name}</span>
                      <span class="team-skills-item-actions">
                        <button
                          type="button"
                          class="team-skill-btn"
                          onclick={() => skillEditIdx === idx ? (skillEditIdx = null) : startEditSkill(idx)}
                        >
                          {skillEditIdx === idx ? "Cancel" : "Edit"}
                        </button>
                        <button
                          type="button"
                          class="team-skill-btn team-skill-danger"
                          onclick={() => removeSkill(idx)}
                        >Delete</button>
                      </span>
                    </div>
                    {#if skillEditIdx === idx}
                      <textarea
                        class="team-skill-editor"
                        bind:value={skillEditContent}
                        rows="8"
                        autocomplete="off"
                        placeholder="Skill content (Markdown)"
                      ></textarea>
                      <button
                        type="button"
                        class="team-skill-btn"
                        onclick={() => saveSkillEdit(idx)}
                      >Save</button>
                    {:else}
                      <pre class="team-skill-preview">{skill.content.slice(0, 120)}{skill.content.length > 120 ? "..." : ""}</pre>
                    {/if}
                  </li>
                {/each}
              </ul>
            {/if}

            {#if skillAddOpen}
              <div class="team-skill-add-form">
                <input
                  type="text"
                  class="team-skill-name-input"
                  bind:value={skillAddName}
                  placeholder="skill-name (e.g. rustacean)"
                  autocomplete="off"
                />
                <textarea
                  class="team-skill-editor"
                  bind:value={skillAddContent}
                  rows="8"
                  autocomplete="off"
                  placeholder="Paste the skill profile content (Markdown)"
                ></textarea>
                <div class="team-skill-add-actions">
                  <button
                    type="button"
                    class="team-skill-btn"
                    disabled={!skillAddName.trim()}
                    onclick={addSkill}
                  >Add skill</button>
                  <button
                    type="button"
                    class="team-skill-btn"
                    onclick={() => { skillAddOpen = false; skillAddName = ""; skillAddContent = ""; }}
                  >Cancel</button>
                </div>
              </div>
            {:else}
              <button
                type="button"
                class="team-skill-btn"
                onclick={() => { skillAddOpen = true; }}
              >+ Add skill</button>
            {/if}
          </div>
        {/if}
      </fieldset>

      <label class="team-field">
        <span class="team-field-label">Number of agents</span>
        <select
          value={config.size}
          onchange={(e) =>
            onSizeChange(Number((e.currentTarget as HTMLSelectElement).value))}
        >
          {#each Array.from({ length: TEAM_MAX_SIZE - TEAM_MIN_SIZE + 1 }, (_, i) => TEAM_MIN_SIZE + i) as n (n)}
            <option value={n}>{n}</option>
          {/each}
        </select>
      </label>

      <fieldset class="team-members">
        <legend>Members</legend>
        {#each config.members as member, idx (idx)}
          {@const assignedCell = cellOfMember(idx)}
          <div
            class="team-member-row"
            class:dragging={draggingMember === idx}
            draggable={config.realEstate.kind === "split"}
            ondragstart={(e) => onMemberDragStart(idx, e)}
            ondragend={onMemberDragEnd}
          >
            <span class="team-member-icon" aria-hidden="true">
              <Bot size={16} strokeWidth={1.75} />
            </span>
            <input
              class="team-member-name"
              bind:value={member.name}
              oninput={(e) =>
                setMemberField(idx, "name", (e.currentTarget as HTMLInputElement).value)}
              placeholder="Name"
              autocomplete="off"
            />
            <input
              class="team-member-command"
              bind:value={member.command}
              oninput={(e) =>
                setMemberField(idx, "command", (e.currentTarget as HTMLInputElement).value)}
              placeholder="claude --resume"
              autocomplete="off"
            />
            <!-- Submit-encoding agent is auto-detected from the command
                 (claude/codex/gemini), overridable with CHAN_AGENT in the env
                 below - no manual picker. -->
            <input
              class="team-member-env"
              bind:value={member.env}
              oninput={(e) =>
                setMemberField(idx, "env", (e.currentTarget as HTMLInputElement).value)}
              placeholder="KEY=value (CHAN_AGENT to override agent)"
              autocomplete="off"
            />
            <label class="team-member-lead">
              <input
                type="radio"
                name="team-lead"
                checked={member.isLead}
                onchange={() => setLead(idx)}
              />
              <span>Lead</span>
            </label>
            {#if config.realEstate.kind === "split"}
              {#if assignedCell !== null}
                <button
                  type="button"
                  class="team-member-cell-badge"
                  title="Unassign from cell"
                  onclick={() => onUnassignClick(idx)}
                >
                  cell {assignedCell + 1}
                </button>
              {:else}
                <span class="team-member-cell-badge unassigned">drag-me</span>
              {/if}
            {/if}
          </div>
          {#if config.skills.length > 0}
            <div class="team-member-skills">
              <span class="team-member-skills-label">Additional skills:</span>
              {#each config.skills as skill (skill.name)}
                <label class="team-member-skill-check">
                  <input
                    type="checkbox"
                    checked={member.skills.includes(skill.name)}
                    onchange={() => toggleMemberSkill(idx, skill.name)}
                  />
                  {skill.name}
                </label>
              {/each}
            </div>
          {/if}
        {/each}
      </fieldset>

      <fieldset class="team-realestate">
        <legend>Real estate</legend>
        <div class="team-realestate-toggle" role="radiogroup" aria-label="Real estate">
          <button
            type="button"
            class="team-realestate-mode"
            class:on={config.realEstate.kind === "tabs"}
            onclick={() => setRealEstate("tabs")}
          >
            Tabs in current Hybrid
          </button>
          <button
            type="button"
            class="team-realestate-mode"
            class:on={config.realEstate.kind === "split"}
            onclick={() => setRealEstate("split")}
          >
            Split panes
          </button>
        </div>

        {#if config.realEstate.kind === "split"}
          {@const shapes = gridShapesForSize(config.size)}
          <div class="team-realestate-shapes" role="radiogroup" aria-label="Grid shapes">
            {#each shapes as shape (`${shape.rows}x${shape.cols}`)}
              <button
                type="button"
                class="team-shape-pick"
                class:on={config.realEstate.kind === "split" &&
                  config.realEstate.grid.rows === shape.rows &&
                  config.realEstate.grid.cols === shape.cols}
                onclick={() => onShapeClick(shape)}
                title={`${shape.rows}x${shape.cols}`}
              >
                {shape.rows}x{shape.cols}
              </button>
            {/each}
            <button
              type="button"
              class="team-auto-assign"
              onclick={autoAssign}
              title="Auto-assign robots across the cells"
              aria-label="Auto-assign robots across the cells"
            >
              <svg viewBox="0 0 24 24" width="14" height="14" fill="none"
                stroke="currentColor" stroke-width="1.8" stroke-linecap="round"
                stroke-linejoin="round" aria-hidden="true">
                <rect x="5" y="9" width="14" height="9" rx="2" />
                <path d="M12 9V5.5" />
                <circle cx="12" cy="4" r="1.3" />
                <circle cx="9.5" cy="13.5" r="1" />
                <circle cx="14.5" cy="13.5" r="1" />
              </svg>
            </button>
          </div>

          <div
            class="team-airplane-grid"
            style:--grid-rows={config.realEstate.grid.rows}
            style:--grid-cols={config.realEstate.grid.cols}
          >
            {#each config.realEstate.slots as cell, cellIdx (cellIdx)}
              <div
                class="team-airplane-cell"
                class:occupied={cell.length > 0}
                ondragover={onCellDragOver}
                ondrop={(e) => onCellDrop(cellIdx, e)}
                role="region"
                aria-label={`Cell ${cellIdx + 1}`}
              >
                <span class="team-cell-index">{cellIdx + 1}</span>
                {#if cell.length === 0}
                  <span class="team-cell-empty">drop bot(s) here</span>
                {:else}
                  <ul class="team-cell-robots">
                    {#each cell as memberIdx (memberIdx)}
                      <li class="team-cell-robot">
                        {handleOf(config.members[memberIdx] ?? {
                          name: `M${memberIdx}`,
                          command: "",
                          env: "",
                          isLead: false,
                        })}
                      </li>
                    {/each}
                  </ul>
                {/if}
              </div>
            {/each}
          </div>

          <p class="team-airplane-hint">
            Drag a robot from the member rows above into a cell.
            Same-cell drop stacks robots as tabs in that pane.
          </p>
        {/if}
      </fieldset>

      {#if submitError}
        <p class="team-dialog-error" role="alert">{submitError}</p>
      {:else if issue}
        <p class="team-dialog-hint">{issue}</p>
      {/if}
    </div>

    <footer class="team-dialog-footer">
      <button type="button" class="team-dialog-cancel" onclick={onCancel} disabled={busy}>
        Cancel
      </button>
      <button
        type="button"
        class="team-dialog-bootstrap"
        onclick={onBootstrap}
        disabled={busy || issue !== null}
      >
        Bootstrap
      </button>
    </footer>
  </div>
</div>

<style>
  .team-dialog-backdrop {
    position: fixed;
    inset: 0;
    background: color-mix(in srgb, var(--bg) 75%, transparent);
    z-index: 50;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .team-dialog {
    background: var(--bg-card);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 8px;
    box-shadow: 0 4px 20px rgba(0, 0, 0, 0.3);
    width: min(720px, 90vw);
    max-height: 90vh;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }
  .team-dialog-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 12px 16px;
    border-bottom: 1px solid var(--border);
  }
  .team-dialog-header h2 {
    margin: 0;
    font-size: 1rem;
    font-weight: 600;
  }
  .team-dialog-close {
    background: transparent;
    border: none;
    color: var(--text-secondary);
    cursor: pointer;
    padding: 4px;
    display: inline-flex;
    align-items: center;
  }
  .team-dialog-body {
    padding: 16px;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .team-field {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .team-field-label {
    font-size: 0.875rem;
    color: var(--text-secondary);
  }
  .team-field input[type="text"],
  .team-field select,
  .team-field textarea {
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 4px;
    padding: 6px 8px;
    color: var(--text);
    font: inherit;
  }
  .team-field textarea {
    resize: vertical;
    min-height: 4.5rem;
  }
  .team-field-hint {
    font-size: 0.75rem;
    color: var(--text-secondary);
  }
  .team-load-error {
    color: var(--danger-text);
  }
  /* Loaded-config surface (TW1): the resolved config.toml path plus a
     one-line team summary, so a Load makes clear what is about to
     bootstrap. */
  .team-load-found {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 4px 8px;
    font-size: 0.75rem;
  }
  .team-load-file {
    color: var(--accent);
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 3px;
    padding: 1px 5px;
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  }
  .team-load-meta {
    color: var(--text-secondary);
  }
  .team-checkbox-row {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 0.875rem;
  }
  .team-members {
    border: 1px solid var(--border);
    border-radius: 4px;
    padding: 8px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .team-members legend {
    padding: 0 6px;
    font-size: 0.75rem;
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  .team-member-row {
    display: grid;
    grid-template-columns: auto 1fr 1.5fr 1fr auto;
    gap: 6px;
    align-items: center;
  }
  .team-member-icon {
    color: var(--text-secondary);
    display: inline-flex;
  }
  .team-member-row :global(input[type="text"]) {
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 4px;
    padding: 4px 6px;
    color: var(--text);
    font: inherit;
    min-width: 0;
  }
  .team-member-lead {
    display: flex;
    align-items: center;
    gap: 4px;
    font-size: 0.75rem;
    color: var(--text-secondary);
  }
  .team-member-row.dragging {
    opacity: 0.6;
  }
  .team-member-cell-badge {
    /* `font-family: inherit` (not the `font` shorthand, which would reset
       font-size back to the larger inherited value) so the 0.7rem below
       sticks; nowrap keeps the badge a single-line pill rather than wrapping
       into a circle under border-radius: 999px. */
    font-family: inherit;
    font-size: 0.7rem;
    white-space: nowrap;
    padding: 2px 6px;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: var(--bg);
    color: var(--text-secondary);
    cursor: pointer;
  }
  .team-member-cell-badge.unassigned {
    cursor: default;
    opacity: 0.6;
  }
  .team-realestate {
    border: 1px solid var(--border);
    border-radius: 4px;
    padding: 8px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .team-realestate legend {
    padding: 0 6px;
    font-size: 0.75rem;
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  .team-realestate-toggle {
    display: flex;
    gap: 6px;
  }
  .team-realestate-mode {
    flex: 1;
    padding: 6px 10px;
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text);
    cursor: pointer;
    font: inherit;
  }
  .team-realestate-mode.on {
    background: color-mix(in srgb, var(--accent) 20%, var(--bg));
    border-color: var(--accent);
  }
  .team-realestate-shapes {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
  }
  .team-shape-pick {
    padding: 4px 10px;
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text);
    cursor: pointer;
    font: inherit;
    font-size: 0.8rem;
  }
  .team-shape-pick.on {
    background: color-mix(in srgb, var(--accent) 20%, var(--bg));
    border-color: var(--accent);
  }
  /* Auto-assign: same pill look as the shape picks, pushed to the right
     end of the shapes row, sized to hold the robot icon. */
  .team-auto-assign {
    margin-left: auto;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    padding: 4px 8px;
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 4px;
    color: var(--text);
    cursor: pointer;
  }
  .team-auto-assign:hover {
    border-color: var(--accent);
    color: var(--accent);
  }
  .team-airplane-grid {
    display: grid;
    grid-template-rows: repeat(var(--grid-rows), 1fr);
    grid-template-columns: repeat(var(--grid-cols), 1fr);
    gap: 4px;
    min-height: 120px;
    padding: 6px;
    background: var(--bg);
    border-radius: 4px;
  }
  .team-airplane-cell {
    position: relative;
    border: 1px dashed var(--border);
    border-radius: 4px;
    padding: 16px 6px 6px;
    min-height: 60px;
    background: var(--bg-card);
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .team-airplane-cell.occupied {
    border-style: solid;
    border-color: var(--accent);
  }
  .team-cell-index {
    position: absolute;
    top: 2px;
    left: 4px;
    font-size: 0.65rem;
    color: var(--text-secondary);
  }
  .team-cell-empty {
    align-self: center;
    margin: auto;
    font-size: 0.75rem;
    color: var(--text-secondary);
    opacity: 0.7;
  }
  .team-cell-robots {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .team-cell-robot {
    font-size: 0.75rem;
    padding: 2px 4px;
    background: var(--bg);
    border-radius: 3px;
    color: var(--text);
  }
  .team-airplane-hint {
    margin: 0;
    font-size: 0.75rem;
    color: var(--text-secondary);
  }
  .team-dialog-error {
    margin: 0;
    color: var(--danger-text);
    font-size: 0.875rem;
  }
  .team-dialog-hint {
    margin: 0;
    color: var(--text-secondary);
    font-size: 0.875rem;
  }
  .team-dialog-footer {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    padding: 12px 16px;
    border-top: 1px solid var(--border);
  }
  .team-dialog-footer button {
    padding: 6px 14px;
    border-radius: 4px;
    cursor: pointer;
    font: inherit;
  }
  .team-dialog-cancel {
    background: var(--btn-bg);
    border: 1px solid var(--btn-border);
    color: var(--text);
  }
  .team-dialog-bootstrap {
    background: var(--accent);
    border: 1px solid var(--accent);
    color: var(--bg);
  }
  .team-dialog-bootstrap:disabled,
  .team-dialog-cancel:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .team-templates-fieldset {
    border: 1px solid var(--border);
    border-radius: 4px;
    padding: 0;
  }
  .team-templates-fieldset legend {
    padding: 0 6px;
    font-size: 0.75rem;
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  .team-templates-toggle {
    background: none;
    border: none;
    color: inherit;
    font: inherit;
    font-size: 0.75rem;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    cursor: pointer;
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 0;
  }
  .team-templates-caret {
    font-style: normal;
  }
  .team-templates-body {
    padding: 8px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .team-templates-empty {
    margin: 0;
    font-size: 0.8rem;
    color: var(--text-secondary);
  }
  .team-templates-list {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .team-templates-item {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    padding: 4px 0;
    border-bottom: 1px solid var(--border);
  }
  .team-templates-item:last-child {
    border-bottom: none;
  }
  .team-templates-name {
    font-size: 0.85rem;
    font-weight: 500;
  }
  .team-templates-meta {
    font-size: 0.75rem;
    color: var(--text-secondary);
    flex: 1;
  }
  .team-templates-actions {
    display: flex;
    gap: 4px;
  }
  .team-tpl-btn {
    padding: 3px 8px;
    font: inherit;
    font-size: 0.75rem;
    border-radius: 4px;
    border: 1px solid var(--border);
    background: var(--bg);
    color: var(--text);
    cursor: pointer;
    text-decoration: none;
    display: inline-flex;
    align-items: center;
  }
  .team-tpl-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .team-tpl-danger {
    color: var(--danger-text, #c0392b);
    border-color: var(--danger-text, #c0392b);
  }
  .team-tpl-save-row {
    display: flex;
    gap: 6px;
    align-items: center;
  }
  .team-tpl-name-input {
    flex: 1;
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 4px;
    padding: 4px 6px;
    color: var(--text);
    font: inherit;
    font-size: 0.85rem;
  }
  .team-tpl-import-row {
    display: flex;
    gap: 6px;
    align-items: center;
    flex-wrap: wrap;
  }
  .team-tpl-import-label {
    cursor: pointer;
  }
  .team-tpl-file-input {
    position: absolute;
    width: 1px;
    height: 1px;
    opacity: 0;
    pointer-events: none;
  }
  .team-tpl-error {
    margin: 0;
    font-size: 0.8rem;
    color: var(--danger-text, #c0392b);
  }

  /* Engineering Standards (skills) fieldset */
  .team-skills-fieldset {
    border: 1px solid var(--border);
    border-radius: 4px;
    padding: 0;
  }
  .team-skills-fieldset legend {
    padding: 0 6px;
    font-size: 0.75rem;
    color: var(--text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  .team-skills-toggle {
    background: none;
    border: none;
    color: inherit;
    font: inherit;
    font-size: 0.75rem;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    cursor: pointer;
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 0;
  }
  .team-skills-count {
    font-style: normal;
    color: var(--accent);
  }
  .team-skills-caret {
    font-style: normal;
  }
  .team-skills-body {
    padding: 8px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .team-skills-empty {
    margin: 0;
    font-size: 0.8rem;
    color: var(--text-secondary);
  }
  .team-skills-list {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .team-skills-item {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 6px 0;
    border-bottom: 1px solid var(--border);
  }
  .team-skills-item:last-child {
    border-bottom: none;
  }
  .team-skills-item-header {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .team-skills-name {
    font-size: 0.85rem;
    font-weight: 500;
    flex: 1;
  }
  .team-skills-item-actions {
    display: flex;
    gap: 4px;
  }
  .team-skill-preview {
    margin: 0;
    font-size: 0.75rem;
    color: var(--text-secondary);
    background: var(--bg);
    border-radius: 4px;
    padding: 4px 6px;
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 3.5rem;
    overflow: hidden;
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  }
  .team-skill-editor {
    width: 100%;
    resize: vertical;
    min-height: 6rem;
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 4px;
    padding: 6px 8px;
    color: var(--text);
    font: inherit;
    font-size: 0.8rem;
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    box-sizing: border-box;
  }
  .team-skill-add-form {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .team-skill-name-input {
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 4px;
    padding: 4px 6px;
    color: var(--text);
    font: inherit;
    font-size: 0.85rem;
  }
  .team-skill-add-actions {
    display: flex;
    gap: 6px;
  }
  .team-skill-btn {
    padding: 3px 8px;
    font: inherit;
    font-size: 0.75rem;
    border-radius: 4px;
    border: 1px solid var(--border);
    background: var(--bg);
    color: var(--text);
    cursor: pointer;
  }
  .team-skill-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .team-skill-danger {
    color: var(--danger-text, #c0392b);
    border-color: var(--danger-text, #c0392b);
  }

  /* Per-member additional skills (shown when team has skills defined) */
  .team-member-skills {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px;
    padding: 2px 0 4px 22px;
  }
  .team-member-skills-label {
    font-size: 0.72rem;
    color: var(--text-secondary);
    white-space: nowrap;
  }
  .team-member-skill-check {
    display: flex;
    align-items: center;
    gap: 3px;
    font-size: 0.75rem;
    color: var(--text-secondary);
    cursor: pointer;
  }
</style>
