<script lang="ts">
  import { onMount } from "svelte";
  import { api, MAIN_QUEUE, type Queue } from "./api";
  import { clock, minutes } from "./format";
  import Icon from "./Icon.svelte";

  let {
    queue,
    onclose,
    oncreated,
  }: {
    /** The queue to edit, or null to create one. */
    queue: Queue | null;
    onclose: () => void;
    oncreated?: (queue: Queue) => void;
  } = $props();

  const DAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
  const ALL_DAYS = 0x7f;

  // The dialog edits a copy, saved on submit.
  // svelte-ignore state_referenced_locally
  const original = queue;
  let name = $state(original?.name ?? "");
  let maxActive = $state(original?.maxActive ?? 3);
  let scheduled = $state(original?.schedule.enabled ?? false);
  let start = $state(clock(original?.schedule.start ?? 2 * 60));
  let stopEnabled = $state(original?.schedule.stop != null);
  let stop = $state(clock(original?.schedule.stop ?? 8 * 60));
  let days = $state(original?.schedule.days ?? ALL_DAYS);
  let busy = $state(false);
  let error = $state("");
  let confirmingDelete = $state(false);
  let dialog: HTMLDialogElement;

  onMount(() => dialog.showModal());

  function toggleDay(i: number) {
    days ^= 1 << i;
  }

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    if (scheduled && days === 0) {
      error = "Pick at least one day for the schedule.";
      return;
    }
    busy = true;
    error = "";
    const edited: Queue = {
      id: original?.id ?? 0,
      name: name.trim(),
      maxActive: Math.min(32, Math.max(1, Math.round(maxActive))),
      schedule: {
        enabled: scheduled,
        start: minutes(start),
        stop: stopEnabled ? minutes(stop) : null,
        days,
      },
    };
    try {
      if (original) await api.updateQueue(edited);
      else oncreated?.(await api.addQueue(edited));
      dialog.close();
    } catch (err) {
      error = String(err);
    } finally {
      busy = false;
    }
  }

  async function remove() {
    if (!original) return;
    busy = true;
    try {
      await api.removeQueue(original.id);
      dialog.close();
    } catch (err) {
      error = String(err);
    } finally {
      busy = false;
    }
  }
</script>

<dialog bind:this={dialog} onclose={onclose} aria-labelledby="queue-title">
  <form onsubmit={submit}>
    <header>
      <h2 id="queue-title">{original ? "Queue settings" : "New queue"}</h2>
      <button type="button" class="icon" onclick={() => dialog.close()} aria-label="Close">
        <Icon name="close" />
      </button>
    </header>

    <div class="inline">
      <label class="grow">
        <span>Name</span>
        <!-- svelte-ignore a11y_autofocus -->
        <input bind:value={name} required autofocus={!original} spellcheck="false" />
      </label>
      <label>
        <span>Downloads at once</span>
        <input type="number" bind:value={maxActive} min="1" max="32" required class="narrow" />
      </label>
    </div>

    <fieldset>
      <legend>
        <label class="check">
          <input type="checkbox" bind:checked={scheduled} />
          Start and stop on a schedule
        </label>
      </legend>
      <div class="inline">
        <label>
          <span>Start at</span>
          <input type="time" bind:value={start} required disabled={!scheduled} />
        </label>
        <label class="check stop">
          <input type="checkbox" bind:checked={stopEnabled} disabled={!scheduled} />
          Stop at
        </label>
        <input
          type="time"
          bind:value={stop}
          required={stopEnabled}
          disabled={!scheduled || !stopEnabled}
          aria-label="Stop at"
        />
      </div>
      <div class="days" role="group" aria-label="Days">
        {#each DAYS as day, i (day)}
          <button
            type="button"
            class="day"
            class:on={(days & (1 << i)) !== 0}
            aria-pressed={(days & (1 << i)) !== 0}
            disabled={!scheduled}
            onclick={() => toggleDay(i)}>{day}</button
          >
        {/each}
      </div>
      <p class="muted hint">
        {#if stopEnabled && minutes(stop) <= minutes(start)}
          Stops the next day. Times are your computer's local time.
        {:else}
          Times are your computer's local time. The app must be running.
        {/if}
      </p>
    </fieldset>

    {#if error}
      <p class="error" role="alert"><Icon name="alert" size={16} />{error}</p>
    {/if}

    <footer>
      {#if original && original.id !== MAIN_QUEUE}
        {#if confirmingDelete}
          <button type="button" class="danger push-left" onclick={remove} disabled={busy}>
            Remove queue and move its downloads to Main
          </button>
        {:else}
          <button type="button" class="push-left" onclick={() => (confirmingDelete = true)}>
            <Icon name="trash" />Remove
          </button>
        {/if}
      {/if}
      <button type="button" onclick={() => dialog.close()}>Cancel</button>
      <button type="submit" class="primary" disabled={busy}>{original ? "Save" : "Create"}</button>
    </footer>
  </form>
</dialog>
