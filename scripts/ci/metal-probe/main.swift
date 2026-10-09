// Metal feasibility probe for GitHub-hosted macOS runners.
//
// Answers one question before any exact-kernel work: can a compute kernel run
// on the Metal device this runner exposes, and how fast does it read memory?
//
// 1. Enumerates MTLCopyAllDevices() and MTLCreateSystemDefaultDevice().
// 2. Prints the device name, maxBufferLength, working-set size, GPU families.
// 3. Compiles a tiny integer kernel from source and checks every output
//    against the same arithmetic on the CPU: char4 dot products in int,
//    simd_sum over int, and a 64-bit multiply with a floor shift.
// 4. Measures read bandwidth (shared and private buffers) and a blit copy,
//    timed with the command buffers' GPU timestamps.
//
// Exit status 0 means Metal compute is usable on this runner. Timings from a
// virtual machine describe that VM, not Apple GPU hardware.

import Foundation
import Metal

func fail(_ message: String) -> Never {
    print("PROBE RESULT: FAIL - \(message)")
    exit(1)
}

struct SplitMix64 {
    var state: UInt64
    mutating func next() -> UInt64 {
        state = state &+ 0x9E37_79B9_7F4A_7C15
        var z = state
        z = (z ^ (z >> 30)) &* 0xBF58_476D_1CE4_E5B9
        z = (z ^ (z >> 27)) &* 0x94D0_49BB_1331_11EB
        return z ^ (z >> 31)
    }
}

let gib = 1024.0 * 1024.0 * 1024.0
print("host: \(ProcessInfo.processInfo.operatingSystemVersionString), "
    + "physical memory \(String(format: "%.2f", Double(ProcessInfo.processInfo.physicalMemory) / gib)) GiB, "
    + "\(ProcessInfo.processInfo.activeProcessorCount) active CPUs")

let all = MTLCopyAllDevices()
print("MTLCopyAllDevices: \(all.count) device(s)")
for (index, candidate) in all.enumerated() {
    print("  [\(index)] \(candidate.name) registryID=\(candidate.registryID) "
        + "lowPower=\(candidate.isLowPower) headless=\(candidate.isHeadless) "
        + "removable=\(candidate.isRemovable) unifiedMemory=\(candidate.hasUnifiedMemory)")
}

guard let device = MTLCreateSystemDefaultDevice() else {
    fail("MTLCreateSystemDefaultDevice() returned nil")
}
print("MTLCreateSystemDefaultDevice: \(device.name)")
if #available(macOS 14.0, *) {
    print("architecture: \(device.architecture.name)")
}
print("maxBufferLength: \(device.maxBufferLength) bytes "
    + "(\(String(format: "%.2f", Double(device.maxBufferLength) / gib)) GiB)")
print("recommendedMaxWorkingSetSize: \(device.recommendedMaxWorkingSetSize) bytes "
    + "(\(String(format: "%.2f", Double(device.recommendedMaxWorkingSetSize) / gib)) GiB)")
print("hasUnifiedMemory: \(device.hasUnifiedMemory)")
print("maxThreadsPerThreadgroup: \(device.maxThreadsPerThreadgroup.width)")
print("maxThreadgroupMemoryLength: \(device.maxThreadgroupMemoryLength) bytes")
var families: [String] = []
for (name, family) in [
    ("apple7", MTLGPUFamily.apple7),
    ("apple8", MTLGPUFamily.apple8),
    ("apple9", MTLGPUFamily.apple9),
    ("mac2", MTLGPUFamily.mac2),
    ("common3", MTLGPUFamily.common3),
] where device.supportsFamily(family) {
    families.append(name)
}
if #available(macOS 13.0, *), device.supportsFamily(.metal3) {
    families.append("metal3")
}
print("GPU families: \(families.joined(separator: ","))")

let source = """
#include <metal_stdlib>
using namespace metal;

// out[3g] = 4-way char dot, out[3g+1] = simd_sum of it,
// out[3g+2] = floor((acc * s) / 2^16) for acc = lane_sum * 2^16 + dot,
// computed with unsigned 64-bit operations and an explicit sign fill.
kernel void int_probe(device const char4 *a [[buffer(0)]],
                      device const char4 *b [[buffer(1)]],
                      device const long *s [[buffer(2)]],
                      device long *out [[buffer(3)]],
                      uint gid [[thread_position_in_grid]])
{
    char4 x = a[gid];
    char4 y = b[gid];
    int dot4 = int(x.x) * int(y.x) + int(x.y) * int(y.y)
             + int(x.z) * int(y.z) + int(x.w) * int(y.w);
    int lane_sum = simd_sum(dot4);
    long acc = long(lane_sum) * 65536 + long(dot4);
    ulong product = as_type<ulong>(acc) * as_type<ulong>(s[gid]);
    ulong shifted = product >> 16;
    if (as_type<long>(product) < 0) {
        shifted |= 0xFFFF000000000000UL;
    }
    out[3 * gid] = long(dot4);
    out[3 * gid + 1] = long(lane_sum);
    out[3 * gid + 2] = as_type<long>(shifted);
}

kernel void read_bw(device const uint4 *src [[buffer(0)]],
                    device uint *sink [[buffer(1)]],
                    constant uint &count [[buffer(2)]],
                    uint gid [[thread_position_in_grid]],
                    uint threads [[threads_per_grid]])
{
    uint4 acc = uint4(0u);
    for (uint i = gid; i < count; i += threads) {
        acc ^= src[i];
    }
    sink[gid] = acc.x ^ acc.y ^ acc.z ^ acc.w;
}
"""

let library: MTLLibrary
do {
    library = try device.makeLibrary(source: source, options: MTLCompileOptions())
} catch {
    fail("MSL compile failed: \(error)")
}
guard let intFunction = library.makeFunction(name: "int_probe"),
      let readFunction = library.makeFunction(name: "read_bw") else {
    fail("kernel functions missing from the compiled library")
}
let intPipeline: MTLComputePipelineState
let readPipeline: MTLComputePipelineState
do {
    intPipeline = try device.makeComputePipelineState(function: intFunction)
    readPipeline = try device.makeComputePipelineState(function: readFunction)
} catch {
    fail("pipeline creation failed: \(error)")
}
guard let queue = device.makeCommandQueue() else {
    fail("makeCommandQueue returned nil")
}
let simdWidth = intPipeline.threadExecutionWidth
print("threadExecutionWidth: \(simdWidth), int_probe maxTotalThreadsPerThreadgroup: "
    + "\(intPipeline.maxTotalThreadsPerThreadgroup)")

// ---- integer kernel, checked element by element against the CPU ----
let n = 1 << 16
let group = 256
var rng = SplitMix64(state: 0x00A2_C0DE)
var a = [Int8](repeating: 0, count: 4 * n)
var b = [Int8](repeating: 0, count: 4 * n)
var s = [Int64](repeating: 0, count: n)
for i in 0..<(4 * n) {
    a[i] = Int8(truncatingIfNeeded: rng.next())
    b[i] = Int8(truncatingIfNeeded: rng.next())
}
// Boundary bytes: -128 * -128 and -128 * 127 in the first lanes.
for i in 0..<16 {
    a[i] = (i % 2 == 0) ? -128 : 127
    b[i] = -128
}
for i in 0..<n {
    s[i] = Int64(rng.next() % (1 << 25)) - (1 << 24)
}
guard let aBuffer = device.makeBuffer(bytes: a, length: a.count, options: .storageModeShared),
      let bBuffer = device.makeBuffer(bytes: b, length: b.count, options: .storageModeShared),
      let sBuffer = device.makeBuffer(bytes: s, length: s.count * 8, options: .storageModeShared),
      let outBuffer = device.makeBuffer(length: 3 * n * 8, options: .storageModeShared) else {
    fail("buffer allocation for the integer kernel failed")
}
guard let intCommands = queue.makeCommandBuffer(),
      let intEncoder = intCommands.makeComputeCommandEncoder() else {
    fail("command buffer creation failed")
}
intEncoder.setComputePipelineState(intPipeline)
intEncoder.setBuffer(aBuffer, offset: 0, index: 0)
intEncoder.setBuffer(bBuffer, offset: 0, index: 1)
intEncoder.setBuffer(sBuffer, offset: 0, index: 2)
intEncoder.setBuffer(outBuffer, offset: 0, index: 3)
intEncoder.dispatchThreadgroups(
    MTLSize(width: n / group, height: 1, depth: 1),
    threadsPerThreadgroup: MTLSize(width: group, height: 1, depth: 1))
intEncoder.endEncoding()
intCommands.commit()
intCommands.waitUntilCompleted()
if intCommands.status != .completed {
    fail("integer kernel did not complete: \(String(describing: intCommands.error))")
}
let gpuOut = outBuffer.contents().bindMemory(to: Int64.self, capacity: 3 * n)
var dots = [Int64](repeating: 0, count: n)
for i in 0..<n {
    var dot: Int64 = 0
    for k in 0..<4 {
        dot += Int64(a[4 * i + k]) * Int64(b[4 * i + k])
    }
    dots[i] = dot
}
var dotMismatches = 0
var sumMismatches = 0
var epilogueMismatches = 0
for start in stride(from: 0, to: n, by: simdWidth) {
    let laneSum = dots[start..<(start + simdWidth)].reduce(0, +)
    for i in start..<(start + simdWidth) {
        let acc = laneSum * 65536 + dots[i]
        let expected = (acc * s[i]) >> 16
        if gpuOut[3 * i] != dots[i] { dotMismatches += 1 }
        if gpuOut[3 * i + 1] != laneSum { sumMismatches += 1 }
        if gpuOut[3 * i + 2] != expected { epilogueMismatches += 1 }
    }
}
print("integer kernel: \(n) threads, mismatches: dot4=\(dotMismatches) "
    + "simd_sum=\(sumMismatches) i64_epilogue=\(epilogueMismatches)")
if dotMismatches + sumMismatches + epilogueMismatches != 0 {
    fail("integer kernel results differ from the CPU")
}

// ---- bandwidth ----
let bandwidthBytes = 256 * 1024 * 1024
let words = bandwidthBytes / 16
let readThreads = 256 * 1024
guard let shared = device.makeBuffer(length: bandwidthBytes, options: .storageModeShared),
      let privateBuffer = device.makeBuffer(length: bandwidthBytes, options: .storageModePrivate),
      let sink = device.makeBuffer(length: readThreads * 4, options: .storageModeShared) else {
    fail("could not allocate \(bandwidthBytes) byte bandwidth buffers")
}
let fill = shared.contents().bindMemory(to: UInt64.self, capacity: bandwidthBytes / 8)
for i in 0..<(bandwidthBytes / 8) {
    fill[i] = rng.next()
}

func runTimed(_ label: String, iterations: Int, encode: (MTLCommandBuffer) -> Void) -> [Double] {
    var times: [Double] = []
    for iteration in 0...iterations {
        guard let commands = queue.makeCommandBuffer() else {
            fail("\(label): command buffer creation failed")
        }
        encode(commands)
        commands.commit()
        commands.waitUntilCompleted()
        if commands.status != .completed {
            fail("\(label): did not complete: \(String(describing: commands.error))")
        }
        if iteration > 0 {
            times.append(commands.gpuEndTime - commands.gpuStartTime)
        }
    }
    return times.sorted()
}

func encodeRead(_ buffer: MTLBuffer) -> (MTLCommandBuffer) -> Void {
    return { commands in
        guard let encoder = commands.makeComputeCommandEncoder() else {
            fail("compute encoder creation failed")
        }
        var count = UInt32(words)
        encoder.setComputePipelineState(readPipeline)
        encoder.setBuffer(buffer, offset: 0, index: 0)
        encoder.setBuffer(sink, offset: 0, index: 1)
        encoder.setBytes(&count, length: MemoryLayout<UInt32>.stride, index: 2)
        encoder.dispatchThreadgroups(
            MTLSize(width: readThreads / group, height: 1, depth: 1),
            threadsPerThreadgroup: MTLSize(width: group, height: 1, depth: 1))
        encoder.endEncoding()
    }
}

let copyTimes = runTimed("blit copy", iterations: 5) { commands in
    guard let blit = commands.makeBlitCommandEncoder() else {
        fail("blit encoder creation failed")
    }
    blit.copy(from: shared, sourceOffset: 0, to: privateBuffer, destinationOffset: 0, size: bandwidthBytes)
    blit.endEncoding()
}
let sharedTimes = runTimed("read shared", iterations: 10, encode: encodeRead(shared))

// The read kernel must have read every word: check its XOR fold on the CPU.
let words32 = shared.contents().bindMemory(to: UInt32.self, capacity: bandwidthBytes / 4)
let sinkWords = sink.contents().bindMemory(to: UInt32.self, capacity: readThreads)
var foldMismatches = 0
for thread in 0..<readThreads {
    var fold: UInt32 = 0
    var index = thread
    while index < words {
        fold ^= words32[4 * index] ^ words32[4 * index + 1] ^ words32[4 * index + 2] ^ words32[4 * index + 3]
        index += readThreads
    }
    if fold != sinkWords[thread] { foldMismatches += 1 }
}
if foldMismatches != 0 {
    fail("read kernel XOR fold differs from the CPU in \(foldMismatches) threads")
}
let privateTimes = runTimed("read private", iterations: 10, encode: encodeRead(privateBuffer))

func rate(_ bytes: Int, _ seconds: Double) -> String {
    return String(format: "%.1f", Double(bytes) / seconds / 1e9)
}
func ms(_ seconds: Double) -> String {
    return String(format: "%.3f", seconds * 1e3)
}
let readShared = rate(bandwidthBytes, sharedTimes[0])
let readPrivate = rate(bandwidthBytes, privateTimes[0])
print("read 256 MiB, shared buffer: best \(ms(sharedTimes[0])) ms (\(readShared) GB/s), "
    + "median \(ms(sharedTimes[sharedTimes.count / 2])) ms "
    + "(\(rate(bandwidthBytes, sharedTimes[sharedTimes.count / 2])) GB/s)")
print("read 256 MiB, private buffer: best \(ms(privateTimes[0])) ms (\(readPrivate) GB/s), "
    + "median \(ms(privateTimes[privateTimes.count / 2])) ms "
    + "(\(rate(bandwidthBytes, privateTimes[privateTimes.count / 2])) GB/s)")
print("blit copy 256 MiB shared->private: best \(ms(copyTimes[0])) ms "
    + "(\(rate(bandwidthBytes, copyTimes[0])) GB/s copied)")
print("PROBE RESULT: PASS - Metal compute usable. device=\(device.name) "
    + "maxBufferLength=\(device.maxBufferLength) "
    + "read_GBps_shared=\(readShared) read_GBps_private=\(readPrivate) "
    + "integer_kernel_mismatches=0 (virtualized-runner measurement)")
