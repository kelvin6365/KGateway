"use client";

// Global ambient background: one fixed, full-viewport WebGL canvas rendering
// the "midnight silk" shader behind every page. Raw WebGL (no library) — a
// single fullscreen triangle + fragment shader is all this needs.
//
// Behavior contract:
//   - prefers-reduced-motion: renders exactly ONE frame at u_time=0, no rAF loop
//   - pauses the rAF loop while the tab is hidden (visibilitychange)
//   - devicePixelRatio capped at 2 (backing store only; CSS size stays 100%)
//   - WebGL unavailable: canvas stays transparent, body background shows through
//   - theme-aware: watches the `dark` class on <html> and blends the shader
//     palette via u_light (0 = midnight, 1 = porcelain)

import { useEffect, useRef } from "react";
import { usePrefersReducedMotion } from "./use-reduced-motion";
import { fragmentSrc, vertexSrc } from "./shaders/silk";

function compile(gl: WebGLRenderingContext, type: number, src: string): WebGLShader | null {
  const shader = gl.createShader(type);
  if (!shader) return null;
  gl.shaderSource(shader, src);
  gl.compileShader(shader);
  if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
    console.warn("baroque-bg shader compile failed:", gl.getShaderInfoLog(shader));
    gl.deleteShader(shader);
    return null;
  }
  return shader;
}

export function WebGLBackground() {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const reducedMotion = usePrefersReducedMotion();

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;

    const gl = (canvas.getContext("webgl2") ??
      canvas.getContext("webgl")) as WebGLRenderingContext | null;
    if (!gl) return;

    const vs = compile(gl, gl.VERTEX_SHADER, vertexSrc);
    const fs = compile(gl, gl.FRAGMENT_SHADER, fragmentSrc);
    if (!vs || !fs) return;
    const program = gl.createProgram();
    if (!program) return;
    gl.attachShader(program, vs);
    gl.attachShader(program, fs);
    gl.linkProgram(program);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
      console.warn("baroque-bg program link failed:", gl.getProgramInfoLog(program));
      return;
    }
    gl.useProgram(program);

    // One triangle covering the whole clip space — no index buffer needed.
    const buf = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, buf);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 3, -1, -1, 3]), gl.STATIC_DRAW);
    const aPosition = gl.getAttribLocation(program, "a_position");
    gl.enableVertexAttribArray(aPosition);
    gl.vertexAttribPointer(aPosition, 2, gl.FLOAT, false, 0, 0);

    const uTime = gl.getUniformLocation(program, "u_time");
    const uResolution = gl.getUniformLocation(program, "u_resolution");
    const uLight = gl.getUniformLocation(program, "u_light");

    // 0 = midnight (.dark on <html>), 1 = porcelain. Read live so a theme
    // toggle repaints without re-creating the GL context.
    const isLight = () => (document.documentElement.classList.contains("dark") ? 0 : 1);

    function resize() {
      if (!canvas || !gl) return;
      const dpr = Math.min(window.devicePixelRatio || 1, 2);
      const w = Math.round(canvas.clientWidth * dpr);
      const h = Math.round(canvas.clientHeight * dpr);
      if (canvas.width !== w || canvas.height !== h) {
        canvas.width = w;
        canvas.height = h;
        gl.viewport(0, 0, w, h);
      }
    }

    function draw(timeSec: number) {
      if (!gl) return;
      resize();
      gl.uniform1f(uTime, timeSec);
      gl.uniform2f(uResolution, canvas!.width, canvas!.height);
      gl.uniform1f(uLight, isLight());
      gl.drawArrays(gl.TRIANGLES, 0, 3);
    }

    let raf = 0;
    let running = false;
    const start = performance.now();

    function loop() {
      draw((performance.now() - start) / 1000);
      raf = requestAnimationFrame(loop);
    }

    function startLoop() {
      if (running || reducedMotion) return;
      running = true;
      raf = requestAnimationFrame(loop);
    }

    function stopLoop() {
      running = false;
      cancelAnimationFrame(raf);
    }

    function onVisibility() {
      if (document.hidden) stopLoop();
      else startLoop();
    }

    function onResize() {
      // Under reduced motion there is no loop — redraw the static frame.
      if (reducedMotion) draw(0);
    }

    if (reducedMotion) {
      draw(0); // one fixed frame, never animated
    } else {
      startLoop();
    }

    // Theme toggles flip the `dark` class on <html>; the animation loop picks
    // the change up on its next frame, but under reduced motion we must
    // repaint the single static frame ourselves.
    const themeObserver = new MutationObserver(() => {
      if (reducedMotion) draw(0);
    });
    themeObserver.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["class"],
    });

    document.addEventListener("visibilitychange", onVisibility);
    window.addEventListener("resize", onResize);
    return () => {
      stopLoop();
      themeObserver.disconnect();
      document.removeEventListener("visibilitychange", onVisibility);
      window.removeEventListener("resize", onResize);
      gl.deleteProgram(program);
      gl.deleteShader(vs);
      gl.deleteShader(fs);
      gl.deleteBuffer(buf);
    };
  }, [reducedMotion]);

  return (
    <canvas
      ref={canvasRef}
      aria-hidden
      className="pointer-events-none fixed inset-0 z-0 h-full w-full"
    />
  );
}
