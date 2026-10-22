<script lang="ts">
  import { onMount } from "svelte";
  import Details from "./Details.svelte";
  import type { Item } from "./downloads.svelte";
  import FileIcon from "./FileIcon.svelte";
  import Icon from "./Icon.svelte";

  let {
    item,
    onclose,
    onlimit,
    onaddress,
  }: { item: Item; onclose: () => void; onlimit: () => void; onaddress: () => void } = $props();

  let dialog: HTMLDialogElement;

  onMount(() => dialog.showModal());
</script>

<dialog bind:this={dialog} class="properties" onclose={onclose} aria-labelledby="properties-title">
  <!-- Not submitted: the links in the details are buttons too. -->
  <form onsubmit={(e) => e.preventDefault()}>
    <header>
      <span class="title">
        <FileIcon {item} />
        <h2 id="properties-title" class="selectable">{item.fileName}</h2>
      </span>
      <button type="button" class="icon" onclick={() => dialog.close()} aria-label="Close">
        <Icon name="close" />
      </button>
    </header>
    <Details {item} {onlimit} {onaddress} />
    <footer>
      <button type="button" class="primary" onclick={() => dialog.close()}>Close</button>
    </footer>
  </form>
</dialog>
