# macOS platform

The current macOS implementation is Viewer-only. It uses VideoToolbox for
H.264 decoding (with OpenH264 fallback) and minifb's Metal-backed MTKView for
presentation.

Video arrives as MTU-sized QUIC DATAGRAM fragments. After media loss, the
Viewer discards dependent inter frames, requests a fresh keyframe, and rebuilds
the decoder chain before resuming. The Cocoa adapter keeps the MTKView inside
the content layout below the title bar and reports view-relative pointer
coordinates; the UI then removes letterbox/pillarbox offsets before mapping to
remote desktop pixels.
