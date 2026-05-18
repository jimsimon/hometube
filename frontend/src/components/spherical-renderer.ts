/**
 * Spherical (360°) video renderer using Three.js.
 *
 * Takes a `<video>` element playing flat equirectangular frames and renders
 * them onto the inside of a sphere, allowing the user to look around with
 * mouse drag or touch drag. The `<canvas>` is sized to fill its container.
 *
 * This module is **lazy-loaded** (dynamic import) so the Three.js bundle
 * cost (~150 KiB gzipped) is only paid for 360° videos.
 */

import {
  BackSide,
  Mesh,
  MeshBasicMaterial,
  PerspectiveCamera,
  Scene,
  SphereGeometry,
  VideoTexture,
  WebGLRenderer,
} from "three";

/** Options for creating a spherical renderer. */
export interface SphericalRendererOptions {
  /** The `<video>` element providing equirectangular frames. */
  video: HTMLVideoElement;
  /** The `<canvas>` element to render into. */
  canvas: HTMLCanvasElement;
  /**
   * Element that receives pointer/wheel events for drag and zoom.
   * Defaults to `canvas`. Pass the player container element when the
   * canvas is covered by a UI overlay (e.g. Shaka controls) that would
   * otherwise swallow pointer events.
   */
  dragTarget?: HTMLElement;
  /** Initial horizontal view angle in degrees (0 = center). */
  initialYaw?: number;
  /** Initial vertical view angle in degrees (0 = horizon). */
  initialPitch?: number;
}

/** A running spherical renderer instance. Call `destroy()` to clean up. */
export interface SphericalRenderer {
  /** Remove all Three.js resources and event listeners. */
  destroy(): void;
  /** Resize the renderer to match the current canvas container size. */
  resize(): void;
}

/**
 * Create and start a spherical renderer.
 *
 * The renderer runs a `requestAnimationFrame` loop that reads the latest
 * decoded video frame from the `<video>` element (via `VideoTexture`) and
 * draws it onto a sphere viewed from inside. The user rotates the camera
 * via pointer drag (mouse or touch).
 */
export function createSphericalRenderer(opts: SphericalRendererOptions): SphericalRenderer {
  const { video, canvas } = opts;
  const dragTarget = opts.dragTarget ?? canvas;

  // Camera orientation in degrees.
  let lon = opts.initialYaw ?? 0;
  let lat = opts.initialPitch ?? 0;
  // Field of view (zoom).
  let fov = 75;

  const scene = new Scene();
  const camera = new PerspectiveCamera(fov, 1, 0.1, 1100);
  const renderer = new WebGLRenderer({ canvas, alpha: false, antialias: false });
  renderer.setPixelRatio(Math.min(window.devicePixelRatio, 2));

  // Sphere with video texture mapped to the inside surface.
  // BackSide renders the interior faces so the camera sees the texture
  // from inside. The equirectangular image appears mirrored when
  // viewed this way; we correct that by negating the longitude in the
  // camera lookAt calculation rather than flipping the geometry (which
  // would also flip the winding order and interact badly with BackSide).
  const geometry = new SphereGeometry(500, 64, 32);
  const texture = new VideoTexture(video);
  // BackSide mirrors the U coordinate, making text read backwards.
  // Flip the texture's repeat.x to -1 and offset.x to 1 to undo that.
  texture.repeat.x = -1;
  texture.offset.x = 1;
  const material = new MeshBasicMaterial({ map: texture, side: BackSide });
  const sphere = new Mesh(geometry, material);
  scene.add(sphere);

  // --- Pointer drag controls ---
  // We use a dead-zone approach: pointerdown records the start position
  // but does NOT capture the pointer or set isDragging. Only once the
  // pointer moves beyond DRAG_THRESHOLD_PX do we capture and start
  // rotating. If the pointer is released before that, the event passes
  // through to Shaka as a normal click (play/pause toggle).
  const DRAG_THRESHOLD_PX = 4;
  let isDragging = false;
  let pointerActive = false;
  let startX = 0;
  let startY = 0;
  let pointerX = 0;
  let pointerY = 0;
  let activePointerId = -1;

  function onPointerDown(e: PointerEvent) {
    // Don't hijack clicks on Shaka control buttons.
    const target = e.target as HTMLElement;
    if (target.closest?.("button, [role=slider], .shaka-overflow-menu")) return;
    pointerActive = true;
    isDragging = false;
    startX = e.clientX;
    startY = e.clientY;
    pointerX = e.clientX;
    pointerY = e.clientY;
    activePointerId = e.pointerId;
    // Do NOT capture yet — let the click propagate if no drag occurs.
  }

  function onPointerMove(e: PointerEvent) {
    if (!pointerActive || e.pointerId !== activePointerId) return;
    if (!isDragging) {
      // Check dead zone.
      const distSq = (e.clientX - startX) ** 2 + (e.clientY - startY) ** 2;
      if (distSq < DRAG_THRESHOLD_PX * DRAG_THRESHOLD_PX) return;
      // Exceeded threshold — start dragging and capture.
      isDragging = true;
      dragTarget.setPointerCapture(e.pointerId);
    }
    const dx = e.clientX - pointerX;
    const dy = e.clientY - pointerY;
    pointerX = e.clientX;
    pointerY = e.clientY;
    // Scale drag speed by FOV so zoomed-in views move proportionally.
    const speed = fov / 600;
    lon -= dx * speed;
    lat += dy * speed;
    lat = Math.max(-85, Math.min(85, lat));
  }

  function onPointerUp(e: PointerEvent) {
    if (!pointerActive || e.pointerId !== activePointerId) return;
    if (isDragging) {
      dragTarget.releasePointerCapture(e.pointerId);
    }
    isDragging = false;
    pointerActive = false;
    activePointerId = -1;
  }

  function onWheel(e: WheelEvent) {
    // Don't interfere with scrolling on control elements.
    const target = e.target as HTMLElement;
    if (target.closest?.("button, [role=slider], .shaka-overflow-menu")) return;
    e.preventDefault();
    fov += e.deltaY * 0.05;
    fov = Math.max(30, Math.min(120, fov));
    camera.fov = fov;
    camera.updateProjectionMatrix();
  }

  dragTarget.addEventListener("pointerdown", onPointerDown);
  dragTarget.addEventListener("pointermove", onPointerMove);
  dragTarget.addEventListener("pointerup", onPointerUp);
  dragTarget.addEventListener("pointercancel", onPointerUp);
  dragTarget.addEventListener("wheel", onWheel, { passive: false });

  // --- Resize handling ---
  function resize() {
    const w = canvas.clientWidth;
    const h = canvas.clientHeight;
    if (w === 0 || h === 0) return;
    renderer.setSize(w, h, false);
    camera.aspect = w / h;
    camera.updateProjectionMatrix();
  }
  resize();

  const resizeObserver = new ResizeObserver(() => resize());
  resizeObserver.observe(canvas);

  // --- Render loop ---
  let animId = 0;
  let destroyed = false;

  function animate() {
    if (destroyed) return;
    animId = requestAnimationFrame(animate);

    // Convert lon/lat to a look-at target on the sphere.
    const phi = ((90 - lat) * Math.PI) / 180;
    const theta = (lon * Math.PI) / 180;
    camera.lookAt(
      500 * Math.sin(phi) * Math.cos(theta),
      500 * Math.cos(phi),
      500 * Math.sin(phi) * Math.sin(theta),
    );

    renderer.render(scene, camera);
  }
  animate();

  // --- Cleanup ---
  function destroy() {
    destroyed = true;
    cancelAnimationFrame(animId);
    resizeObserver.disconnect();
    dragTarget.removeEventListener("pointerdown", onPointerDown);
    dragTarget.removeEventListener("pointermove", onPointerMove);
    dragTarget.removeEventListener("pointerup", onPointerUp);
    dragTarget.removeEventListener("pointercancel", onPointerUp);
    dragTarget.removeEventListener("wheel", onWheel);
    material.dispose();
    texture.dispose();
    geometry.dispose();
    renderer.dispose();
  }

  return { destroy, resize };
}
