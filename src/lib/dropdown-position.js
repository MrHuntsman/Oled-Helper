// Collision-aware positioning for custom dropdown/popover menus.
//
// `updateDropdownPlacement` handles one measurement. The controller below adds
// the common requestAnimationFrame, resize, scroll, and cleanup behavior so a
// future dropdown only needs to provide trigger/menu getters and a placement
// callback.
export function updateDropdownPlacement(triggerEl, menuEl, options = {}) {
  if (!triggerEl || !menuEl) return 'down-right';
  const gap = Number.isFinite(options.gap) ? options.gap : 6;
  const viewportMargin = Number.isFinite(options.viewportMargin) ? options.viewportMargin : 8;
  const maximumHeight = Number.isFinite(options.maxHeight) ? options.maxHeight : 340;
  menuEl.style.setProperty('--dropdown-gap', `${gap}px`);
  menuEl.style.maxHeight = `${maximumHeight}px`;

  const triggerRect = triggerEl.getBoundingClientRect();
  const menuRect = menuEl.getBoundingClientRect();
  const viewportWidth = document.documentElement.clientWidth || window.innerWidth;
  const viewportHeight = document.documentElement.clientHeight || window.innerHeight;
  const belowSpace = Math.max(0, viewportHeight - triggerRect.bottom - viewportMargin - gap);
  const aboveSpace = Math.max(0, triggerRect.top - viewportMargin - gap);
  const opensUp = menuRect.height > belowSpace && aboveSpace > belowSpace;

  const rightAlignedLeft = triggerRect.right - menuRect.width;
  const leftAlignedRight = triggerRect.left + menuRect.width;
  const rightFits = rightAlignedLeft >= viewportMargin
    && triggerRect.right <= viewportWidth - viewportMargin;
  const leftFits = triggerRect.left >= viewportMargin
    && leftAlignedRight <= viewportWidth - viewportMargin;
  const alignment = rightFits ? 'right' : leftFits ? 'left' : 'center';

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
