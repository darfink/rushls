// Loopback audio for check-av-sync.py, which cannot read Safari's audio from
// the page: WebKit gives WebAudio silence for HLS.
//
//     loopback get                       print the default output device
//     loopback set "BlackHole 2ch"       make a device the default output
//     loopback listen "BlackHole 2ch" 12 print beep onsets for 12 seconds
//
// `listen` prints one Unix time in seconds per onset: the first loud sample
// after half a second of quiet, timed from the device's own capture clock.
import AVFoundation
import CoreAudio
import Foundation

let system = AudioObjectID(kAudioObjectSystemObject)

func address(_ selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress(
        mSelector: selector, mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain)
}

func defaultOutput() -> AudioObjectID {
    var device = AudioObjectID(0)
    var where_ = address(kAudioHardwarePropertyDefaultOutputDevice)
    var size = UInt32(MemoryLayout<AudioObjectID>.size)
    AudioObjectGetPropertyData(system, &where_, 0, nil, &size, &device)
    return device
}

func devices() -> [AudioObjectID] {
    var where_ = address(kAudioHardwarePropertyDevices)
    var size: UInt32 = 0
    AudioObjectGetPropertyDataSize(system, &where_, 0, nil, &size)
    var ids = [AudioObjectID](repeating: 0, count: Int(size) / MemoryLayout<AudioObjectID>.size)
    AudioObjectGetPropertyData(system, &where_, 0, nil, &size, &ids)
    return ids
}

func name(_ device: AudioObjectID) -> String {
    var where_ = address(kAudioObjectPropertyName)
    var size = UInt32(MemoryLayout<CFString?>.size)
    var value: CFString? = nil
    let status = withUnsafeMutablePointer(to: &value) {
        AudioObjectGetPropertyData(device, &where_, 0, nil, &size, $0)
    }
    return status == noErr ? (value as String? ?? "") : ""
}

func fail(_ message: String) -> Never {
    FileHandle.standardError.write((message + "\n").data(using: .utf8)!)
    exit(1)
}

func device(named wanted: String) -> AudioObjectID {
    guard let found = devices().first(where: { name($0) == wanted }) else {
        fail("no audio device named \(wanted)")
    }
    return found
}

func listen(to input: AudioObjectID, for seconds: Double) {
    var rate: Float64 = 0
    var rateAddress = address(kAudioDevicePropertyNominalSampleRate)
    var size = UInt32(MemoryLayout<Float64>.size)
    AudioObjectGetPropertyData(input, &rateAddress, 0, nil, &size, &rate)
    if rate <= 0 { fail("the input device has no sample rate") }
    // One reading of both clocks maps host time to Unix time.
    let unixAtStart = Date().timeIntervalSince1970
    let hostAtStart = AVAudioTime.seconds(forHostTime: mach_absolute_time())
    var quiet = 0
    var procedure: AudioDeviceIOProcID?
    // An IO proc on the device itself, rather than AVAudioEngine's input
    // node: it needs no default-input switch, which the engine applies
    // asynchronously and may silently drop its tap over.
    let status = AudioDeviceCreateIOProcIDWithBlock(&procedure, input, nil) { _, data, time, _, _ in
        let buffers = UnsafeMutableAudioBufferListPointer(UnsafeMutablePointer(mutating: data))
        guard let first = buffers.first, let raw = first.mData,
            time.pointee.mFlags.contains(.hostTimeValid)
        else { return }
        // HAL buffers are interleaved Float32; channel 0 is enough.
        let channels = Int(max(first.mNumberChannels, 1))
        let frames = Int(first.mDataByteSize) / MemoryLayout<Float32>.size / channels
        let samples = raw.assumingMemoryBound(to: Float32.self)
        let start = AVAudioTime.seconds(forHostTime: time.pointee.mHostTime)
        for frame in 0..<frames {
            if abs(samples[frame * channels]) > 0.2 {
                if quiet > Int(rate / 2) {
                    let unix = unixAtStart + start - hostAtStart + Double(frame) / rate
                    print(String(format: "%.6f", unix))
                    fflush(stdout)
                }
                quiet = 0
            } else {
                quiet += 1
            }
        }
    }
    guard status == noErr, let procedure else { fail("cannot open the input device (\(status))") }
    if AudioDeviceStart(input, procedure) != noErr { fail("cannot start recording") }
    Thread.sleep(forTimeInterval: seconds)
    AudioDeviceStop(input, procedure)
    AudioDeviceDestroyIOProcID(input, procedure)
}

let arguments = CommandLine.arguments
switch (arguments.count, arguments.dropFirst().first) {
case (2, "get"):
    print(name(defaultOutput()))
case (3, "set"):
    var target = device(named: arguments[2])
    var where_ = address(kAudioHardwarePropertyDefaultOutputDevice)
    let status = AudioObjectSetPropertyData(
        system, &where_, 0, nil, UInt32(MemoryLayout<AudioObjectID>.size), &target)
    if status != noErr { fail("cannot set the default output (\(status))") }
case (4, "listen"):
    guard let seconds = Double(arguments[3]) else { fail("seconds must be a number") }
    listen(to: device(named: arguments[2]), for: seconds)
default:
    fail("usage: loopback get | set <device> | listen <device> <seconds>")
}
