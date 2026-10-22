<script lang="ts" module>
  import type { IconName } from "./Icon.svelte";

  export type MenuItem =
    | {
        label: string;
        icon?: IconName;
        /** A keyboard shortcut, shown on the right. */
        hint?: string;
        disabled?: boolean;
        /** Shows a check mark; the item is then a choice that is on or off. */
        checked?: boolean;
        danger?: boolean;
        action?: () => void;
        /** Opens a submenu instead of acting. */
        items?: MenuItem[];
      }
    | "separator";
</script>

<script lang="ts">
  import { onMount } from "svelte";
  import ContextMenu from "./ContextMenu.svelte";
  import Icon from "./Icon.svelte";

  let {
    x,
    y,
    items,
    onclose,
    ondone = onclose,
    flipX,
    sub = false,
  }: {
    x: number;
    y: number;
    items: MenuItem[];
    /** Closes this menu. */
    onclose: () => void;
    /** Closes the whole menu, submenus and all, after an item was picked. */
    ondone?: () => void;
    /** Where a submenu goes when it doesn't fit right of its item: that item's left edge. */
    flipX?: number;
    sub?: boolean;
  } = $props();

  let menu: HTMLDivElement;
  // svelte-ignore state_referenced_locally
  let left = $state(x);
  // svelte-ignore state_referenced_locally
  let top = $state(y);
  /** The item whose submenu is open. */
  let open = $state<{ index: number; x: number; y: number; flipX: number } | null>(null);

  onMount(() => {
    // Keeps the menu inside the window.
    const { width, height } = menu.getBoundingClientRect();
    if (x + width > innerWidth - 4) left = flipX !== undefined ? flipX - width : innerWidth - width - 4;
    left = Math.max(4, left);
    top = Math.max(4, Math.min(y, innerHeight - height - 4));
    focusable()[0]?.focus();
  });

  function focusable(): HTMLButtonElement[] {
    return Array.from(menu.querySelectorAll<HTMLButtonElement>(":scope > button:not(:disabled)"));
  }

  function openSubmenu(index: number, button: HTMLElement) {
    const rect = button.getBoundingClientRect();
    open = { index, x: rect.right - 2, y: rect.top - 5, flipX: rect.left + 2 };
  }

  function choose(index: number, item: Exclude<MenuItem, "separator">, button: HTMLElement) {
    if (item.items) {
      openSubmenu(index, button);
      return;
    }
    ondone();
    item.action?.();
  }

  function hover(index: number, item: Exclude<MenuItem, "separator">, button: HTMLElement) {
    button.focus();
    if (item.items && !item.disabled) openSubmenu(index, button);
    else open = null;
  }

  function keydown(e: KeyboardEvent) {
    const buttons = focusable();
    const at = buttons.indexOf(document.activeElement as HTMLButtonElement);
    const move = (to: number) => buttons[(to + buttons.length) % buttons.length]?.focus();
    switch (e.key) {
      case "ArrowDown":
        move(at + 1);
        break;
      case "ArrowUp":
        move(at < 0 ? -1 : at - 1);
        break;
      case "Home":
        move(0);
        break;
      case "End":
        move(-1);
        break;
      case "ArrowRight": {
        const button = buttons[at];
        const index = Number(button?.dataset.index);
        const item = items[index];
        if (button && item !== "separator" && item?.items) openSubmenu(index, button);
        break;
      }
      case "ArrowLeft":
        if (sub) onclose();
        break;
      case "Escape":
        if (sub) onclose();
        else ondone();
        break;
      case "Tab":
        ondone();
        break;
      default:
        return;
    }
    e.preventDefault();
    e.stopPropagation();
  }

  /** Clicks outside every open menu close them. */
  function pointerdown(e: PointerEvent) {
    if (!sub && !(e.target as Element).closest?.(".context-menu")) ondone();
  }

  function closeAll() {
    if (!sub) ondone();
  }
</script>

<svelte:window
  onpointerdown={pointerdown}
  onblur={closeAll}
  onresize={closeAll}
  onwheel={closeAll}
/>

<div
  class="context-menu"
  role="menu"
  tabindex="-1"
  bind:this={menu}
  style="left: {left}px; top: {top}px"
  onkeydown={keydown}
  oncontextmenu={(e) => e.preventDefault()}
>
  {#each items as item, index (index)}
    {#if item === "separator"}
      <div class="separator" role="separator"></div>
    {:else}
      <button
        type="button"
        role={item.checked === undefined ? "menuitem" : "menuitemcheckbox"}
        aria-checked={item.checked}
        aria-haspopup={item.items ? "menu" : undefined}
        aria-expanded={item.items ? open?.index === index : undefined}
        class:danger={item.danger}
        class:expanded={open?.index === index}
        disabled={item.disabled}
        data-index={index}
        onclick={(e) => choose(index, item, e.currentTarget)}
        onmouseenter={(e) => hover(index, item, e.currentTarget)}
      >
        <span class="mark">
          {#if item.checked}
            <Icon name="check" size={15} />
          {:else if item.icon}
            <Icon name={item.icon} size={15} />
          {/if}
        </span>
        <span class="label">{item.label}</span>
        {#if item.hint}<span class="key">{item.hint}</span>{/if}
        {#if item.items}<span class="arrow" aria-hidden="true">›</span>{/if}
      </button>
    {/if}
  {/each}
</div>

{#if open}
  {@const parent = items[open.index]}
  {#if parent !== "separator" && parent.items}
    <ContextMenu
      x={open.x}
      y={open.y}
      flipX={open.flipX}
      items={parent.items}
      sub
      onclose={() => {
        const index = open?.index;
        open = null;
        menu.querySelector<HTMLElement>(`[data-index="${index}"]`)?.focus();
      }}
      {ondone}
    />
  {/if}
{/if}
