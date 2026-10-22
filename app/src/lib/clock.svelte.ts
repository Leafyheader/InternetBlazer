// The current time in Unix seconds, updated every second, for countdowns.

export const clock = $state({ now: Math.floor(Date.now() / 1000) });

setInterval(() => (clock.now = Math.floor(Date.now() / 1000)), 1000);
