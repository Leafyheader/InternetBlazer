<script lang="ts">
  import { onMount } from "svelte";
  import { api, type Countdown, FINISH_LABELS } from "./api";

  /** The app closes this when the action happens or is cancelled elsewhere. */
  let { countdown, onerror }: { countdown: Countdown; onerror: (err: unknown) => void } = $props();

  let dialog: HTMLDialogElement;
  let left = $state(0);

  onMount(() => {
    dialog.showModal();
    // The app does the action when its own countdown ends; this one only shows it.
    const deadline = Date.now() + countdown.seconds * 1000;
    left = countdown.seconds;
    const timer = setInterval(() => (left = Math.max(0, Math.ceil((deadline - Date.now()) / 1000))), 250);
    return () => clearInterval(timer);
  });

  const what = $derived.by(() => {
    switch (countdown.action) {
      case "shutdown":
        return "The computer shuts down";
      case "sleep":
        return "The computer goes to sleep";
      case "hibernate":
        return "The computer hibernates";
      case "quit":
        return "InternetBlazer quits";
    }
  });
</script>

<!-- Closing it any way, Escape included, cancels. -->
<dialog bind:this={dialog} onclose={() => api.cancelFinish().catch(onerror)} aria-labelledby="finish-title">
  <form method="dialog">
    <h2 id="finish-title">{countdown.queue} is done</h2>
    <p aria-live="polite">{what} in <strong>{left}</strong> {left === 1 ? "second" : "seconds"}.</p>
    <p class="muted">This was set for one run of the queue, so it won't happen again.</p>
    <footer>
      <button type="button" onclick={() => api.finishNow().catch(onerror)}>
        {FINISH_LABELS[countdown.action]} now
      </button>
      <!-- svelte-ignore a11y_autofocus -->
      <button type="submit" class="primary" autofocus>Cancel</button>
    </footer>
  </form>
</dialog>
