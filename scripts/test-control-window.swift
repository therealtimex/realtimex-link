// Owned target for scripts/test-macos-control.py: one window that reports, as JSON lines on
// stdout, the input events delivered to it (its own events only, like any app's view) so the
// test can check what rtx-control posted. Stdin: "state" prints whether it is frontmost and key
// and where its pad is (global top-left points); "quit" exits. Test code, never shipped.
import AppKit

func emit(_ fields: [String: Any]) {
  let data = try! JSONSerialization.data(withJSONObject: fields)
  FileHandle.standardOutput.write(data + Data("\n".utf8))
}

let modifierMask: UInt = NSEvent.ModifierFlags.deviceIndependentFlagsMask.rawValue

final class Pad: NSView {
  override var acceptsFirstResponder: Bool { true }
  override func keyDown(with e: NSEvent) {
    emit(["type": "keyDown", "chars": e.characters ?? "", "flags": e.modifierFlags.rawValue & modifierMask])
  }
  override func keyUp(with e: NSEvent) {
    emit(["type": "keyUp", "chars": e.characters ?? "", "flags": e.modifierFlags.rawValue & modifierMask])
  }
  override func flagsChanged(with e: NSEvent) {
    emit(["type": "flags", "flags": e.modifierFlags.rawValue & modifierMask])
  }
  override func mouseDown(with e: NSEvent) { emit(["type": "mouseDown", "button": 0, "clicks": e.clickCount]) }
  override func mouseUp(with e: NSEvent) { emit(["type": "mouseUp", "button": 0]) }
  override func rightMouseDown(with e: NSEvent) { emit(["type": "mouseDown", "button": 1, "clicks": e.clickCount]) }
  override func rightMouseUp(with e: NSEvent) { emit(["type": "mouseUp", "button": 1]) }
  override func scrollWheel(with e: NSEvent) { emit(["type": "scroll", "dy": e.scrollingDeltaY]) }
  override func draw(_ dirtyRect: NSRect) {
    NSColor.systemTeal.setFill()
    dirtyRect.fill()
  }
}

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let window = NSWindow(contentRect: NSRect(x: 300, y: 300, width: 480, height: 320),
                      styleMask: [.titled], backing: .buffered, defer: false)
window.title = "rtx-control-test"
let pad = Pad(frame: window.contentView!.bounds)
window.contentView = pad
window.makeKeyAndOrderFront(nil)
window.makeFirstResponder(pad)
app.activate(ignoringOtherApps: true)

func state() {
  let screenHeight = NSScreen.screens[0].frame.height
  let f = window.convertToScreen(pad.frame)
  emit(["type": "state", "active": app.isActive, "key": window.isKeyWindow,
        "pid": ProcessInfo.processInfo.processIdentifier,
        "pad": [f.origin.x, screenHeight - f.origin.y - f.height, f.width, f.height]])
}

DispatchQueue.global().async {
  while let line = readLine() {
    DispatchQueue.main.async {
      switch line {
      case "state": state()
      case "activate":
        app.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
      case "quit": app.terminate(nil)
      default: break
      }
    }
  }
  DispatchQueue.main.async { app.terminate(nil) }
}
emit(["type": "ready"])
app.run()
