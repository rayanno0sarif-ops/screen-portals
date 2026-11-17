// A pretend user for tests/macos-desktop.sh: moves the mouse, clicks and
// types the way a real mouse and keyboard would, and reports what the Mac
// receives.
//
//     macos-input size                     print "width height" of the main display
//     macos-input where                    print "x y" of the cursor
//     macos-input goto <x> <y>             put the cursor there
//     macos-input move <dx> <dy> <times>   move the mouse, a step at a time
//     macos-input click                    click the left button
//     macos-input key <code>               press and release a key (Mac key code)
//     macos-input chord <code> <mods...>   the same while holding modifier keys
//     macos-input listen <seconds>         print keys and clicks that arrive

import Cocoa

func now() -> CGPoint { CGEvent(source: nil)?.location ?? .zero }

func post(_ event: CGEvent?) {
    event?.post(tap: .cghidEventTap)
    usleep(20_000)
}

// Modifier key code -> (general flag, flag for that side).
let modifiers: [UInt16: UInt64] = [
    0x3B: CGEventFlags.maskControl.rawValue | 0x0001,
    0x3A: CGEventFlags.maskAlternate.rawValue | 0x0020,
    0x38: CGEventFlags.maskShift.rawValue | 0x0002,
    0x37: CGEventFlags.maskCommand.rawValue | 0x0008,
]

func key(_ code: UInt16, _ down: Bool, flags: UInt64) {
    let event = CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: down)
    if modifiers[code] != nil { event?.type = .flagsChanged }
    event?.flags = CGEventFlags(rawValue: flags)
    post(event)
}

let args = CommandLine.arguments.dropFirst().map { $0 }
switch args.first {
case "size":
    let main = CGDisplayBounds(CGMainDisplayID())
    print(Int(main.width), Int(main.height))
case "where":
    let at = now()
    print(Int(at.x), Int(at.y))
case "goto":
    CGWarpMouseCursorPosition(CGPoint(x: Double(args[1])!, y: Double(args[2])!))
    CGAssociateMouseAndMouseCursorPosition(1)
case "move":
    let (dx, dy, times) = (Int64(args[1])!, Int64(args[2])!, Int(args[3])!)
    for _ in 0..<times {
        let at = now()
        let event = CGEvent(mouseEventSource: nil, mouseType: .mouseMoved,
                            mouseCursorPosition: CGPoint(x: at.x + Double(dx), y: at.y + Double(dy)), mouseButton: .left)
        event?.setIntegerValueField(.mouseEventDeltaX, value: dx)
        event?.setIntegerValueField(.mouseEventDeltaY, value: dy)
        post(event)
        usleep(10_000)
    }
case "click":
    let at = now()
    post(CGEvent(mouseEventSource: nil, mouseType: .leftMouseDown, mouseCursorPosition: at, mouseButton: .left))
    post(CGEvent(mouseEventSource: nil, mouseType: .leftMouseUp, mouseCursorPosition: at, mouseButton: .left))
case "key":
    let code = UInt16(args[1], radix: 16)!
    key(code, true, flags: 0)
    key(code, false, flags: 0)
case "chord":
    let code = UInt16(args[1], radix: 16)!
    let held = args.dropFirst(2).map { UInt16($0, radix: 16)! }
    var flags: UInt64 = 0
    for m in held { flags |= modifiers[m]!; key(m, true, flags: flags) }
    key(code, true, flags: flags)
    key(code, false, flags: flags)
    for m in held.reversed() { flags &= ~modifiers[m]!; key(m, false, flags: flags) }
case "listen":
    setvbuf(stdout, nil, _IOLBF, 0)
    let kinds: [CGEventType] = [.keyDown, .keyUp, .leftMouseDown, .leftMouseUp, .flagsChanged]
    let mask = kinds.reduce(CGEventMask(0)) { $0 | (1 << $1.rawValue) }
    let callback: CGEventTapCallBack = { _, type, event, _ in
        switch type {
        case .keyDown: print("key", String(event.getIntegerValueField(.keyboardEventKeycode), radix: 16), "down")
        case .keyUp: print("key", String(event.getIntegerValueField(.keyboardEventKeycode), radix: 16), "up")
        case .leftMouseDown: print("button left down")
        case .leftMouseUp: print("button left up")
        default: print("flags", String(event.flags.rawValue, radix: 16))
        }
        return Unmanaged.passUnretained(event)
    }
    guard let tap = CGEvent.tapCreate(tap: .cgSessionEventTap, place: .tailAppendEventTap, options: .listenOnly,
                                      eventsOfInterest: mask, callback: callback, userInfo: nil) else {
        print("cannot listen"); exit(1)
    }
    CFRunLoopAddSource(CFRunLoopGetCurrent(), CFMachPortCreateRunLoopSource(nil, tap, 0), .commonModes)
    CGEvent.tapEnable(tap: tap, enable: true)
    CFRunLoopRunInMode(.defaultMode, Double(args[1])!, false)
default:
    print("unknown command"); exit(2)
}
