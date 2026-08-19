// Collision-aware positioning for custom dropdown/popover menus.
//
// `updateDropdownPlacement` handles one measurement. The controller below adds
// the common requestAnimationFrame, resize, scroll, and cleanup behavior so a
// future dropdown only needs to provide trigger/menu getters and a placement
// callback.

// The menu is absolutely positioned inside the tab's scrollable content pane,
// so its real clipping edges are that pane's, not the window's: the window
// bottom includes chrome below the pane (status bar, paddings), which would
// let a "fits" menu poke past the pane's visible bottom and summon a
// scrollbar. Walk up to the nearest scrollable ancestor and measure against
// it; fall back to the document element (whose rect is the viewport).
function findScrollContainer(el) {
  let node = el?.parentElement ?? null;
  while (node) {
    const style = getComputedStyle(node);
    if (/(auto|scroll|overlay)/.test(style.overflowY)) return node;
    node = node.parentElement;
  }
  return document.documentElement;
}

export function updateDropdownPlacement(triggerEl, menuEl, options = {}) {
  if (!triggerEl || !menuEl) return 'down-right';
  const gap = Number.isFinite(options.gap) ? options.gap : 6;
  const viewportMargin = Number.isFinite(options.viewportMargin) ? options.viewportMargin : 8;
  const maximumHeight = Number.isFinite(options.maxHeight) ? options.maxHeight : 340;
  menuEl.style.setProperty('--dropdown-gap', `${gap}px`);
  menuEl.style.maxHeight = `${maximumHeight}px`;

  const triggerRect = triggerEl.getBoundingClientRect();
  const menuRect = menuEl.getBoundingClientRect();
  const containerRect = findScrollContainer(menuEl).getBoundingClientRect();
  const viewportWidth = containerRect.width;
  const viewportHeight = containerRect.height;
  const containerTop = containerRect.top;
  const containerBottom = containerRect.bottom;
  const belowSpace = Math.max(0, containerBottom - triggerRect.bottom - viewportMargin - gap);
  const aboveSpace = Math.max(0, triggerRect.top - containerTop - viewportMargin - gap);
  const opensUp = menuRect.height > belowSpace && aboveSpace > belowSpace;

  const rightAlignedLeft = triggerRect.right - menuRect.width;
  const leftAlignedRight = triggerRect.left + menuRect.width;
  const rightFits = rightAlignedLeft >= viewportMargin
    && triggerRect.right <= viewportWidth - viewportMargin;
  const leftFits = triggerRect.left >= viewportMargin
    && leftAlignedRight <= viewportWidth - viewportMargin;
  // A forced alignment (`options.align`) anchors the menu to a specific side
  // of its trigger regardless of the fit check. Used by controls whose
  // dropdown is expected to hang off a particular edge (e.g. a right-side
  // trigger whose menu should stay right-aligned even when the free space
  // to its left is narrower than the menu).
  const alignment = options.align === 'right' || options.align === 'left'
    ? options.align
    : rightFits ? 'right' : leftFits ? 'left' : 'center';

  const availableSpace = opensUp ? aboveSpace : belowSpace;
  menuEl.style.maxHeight = `${Math.max(1, Math.min(maximumHeight, Math.floor(availableSpace)))}px`;
  return `${opensUp ? 'up' : 'down'}-${alignment}`;
}

export function createDropdownPositionController({
  getTrigger,
  getMenu,
  onPlacement,
  options,
}) {
  let frame = 0;

  function reposition() {
    frame = 0;
    const placement = updateDropdownPlacement(getTrigger(), getMenu(), options);
    onPlacement(placement);
  }

  function schedule() {
    cancelAnimationFrame(frame);
    frame = requestAnimationFrame(reposition);
  }

  function onViewportChange() {
    schedule();
  }

  function start() {
    window.addEventListener('resize', onViewportChange);
    // Capture scrolling from nested scroll containers as well as window.
    window.addEventListener('scroll', onViewportChange, true);
    schedule();
  }

  function stop() {
    window.removeEventListener('resize', onViewportChange);
    window.removeEventListener('scroll', onViewportChange, true);
    cancelAnimationFrame(frame);
    frame = 0;
  }

  return { start, stop, schedule };
}
