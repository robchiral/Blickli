import CoreAI
import Foundation

private enum TensorLayout {
    case nchw
    case nhwc
    case chw
    case hwc
}

private let u8ToF32: [Float] = (0..<256).map { Float($0) / 255.0 }
private let u8ToF16: [Float16] = (0..<256).map { Float16(Float($0) / 255.0) }

private enum ScalarKind {
    case float16
    case float32
}

private final class CoreAiModelBox: @unchecked Sendable {
    let model: AIModel
    let function: InferenceFunction
    let inputName: String
    let outputName: String
    let inputShape: [Int]
    let inputLayout: TensorLayout
    let inputScalar: ScalarKind
    let tileWidth: Int
    let tileHeight: Int
    let outputWidth: Int
    let outputHeight: Int
    let scale: Int

    init(
        model: AIModel,
        function: InferenceFunction,
        inputName: String,
        outputName: String,
        inputShape: [Int],
        inputLayout: TensorLayout,
        inputScalar: ScalarKind,
        tileWidth: Int,
        tileHeight: Int,
        outputWidth: Int,
        outputHeight: Int,
        scale: Int
    ) {
        self.model = model
        self.function = function
        self.inputName = inputName
        self.outputName = outputName
        self.inputShape = inputShape
        self.inputLayout = inputLayout
        self.inputScalar = inputScalar
        self.tileWidth = tileWidth
        self.tileHeight = tileHeight
        self.outputWidth = outputWidth
        self.outputHeight = outputHeight
        self.scale = scale
    }
}

private func parseTensorShape(_ shape: [Int]) -> (TensorLayout, Int, Int)? {
    switch shape.count {
    case 4:
        guard shape[0] == 1 else { return nil }
        if shape[1] == 3 {
            return (.nchw, shape[2], shape[3])
        }
        if shape[3] == 3 {
            return (.nhwc, shape[1], shape[2])
        }
    case 3:
        if shape[0] == 3 {
            return (.chw, shape[1], shape[2])
        }
        if shape[2] == 3 {
            return (.hwc, shape[0], shape[1])
        }
    default:
        break
    }
    return nil
}

private func writeError(_ message: String, out: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?) {
    guard let out else { return }
    out.pointee = strdup(message)
}

private final class SendableBox<T>: @unchecked Sendable {
    var value: T
    init(_ value: T) { self.value = value }
}

private func runBlocking<T: Sendable>(_ work: @escaping @Sendable () async throws -> T) -> Result<T, Error> {
    let sem = DispatchSemaphore(value: 0)
    let box = SendableBox<Result<T, Error>?>(nil)
    Task.detached {
        do {
            box.value = .success(try await work())
        } catch {
            box.value = .failure(error)
        }
        sem.signal()
    }
    sem.wait()
    return box.value!
}

@_cdecl("blickli_coreai_free_error")
public func blickli_coreai_free_error(_ ptr: UnsafeMutablePointer<CChar>?) {
    free(ptr)
}

@_cdecl("blickli_coreai_load")
public func blickli_coreai_load(
    _ pathPtr: UnsafePointer<CChar>,
    _ outTileW: UnsafeMutablePointer<Int>,
    _ outTileH: UnsafeMutablePointer<Int>,
    _ outScale: UnsafeMutablePointer<Int>,
    _ outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> UnsafeMutableRawPointer? {
    let path = String(cString: pathPtr)
    let url = URL(fileURLWithPath: path)

    let loaded = runBlocking { () async throws -> CoreAiModelBox in
        let model = try await AIModel.specialize(contentsOf: url)
        let fnName = model.functionNames.contains("main")
            ? "main"
            : (model.functionNames.first ?? "main")
        guard let fn = try model.loadFunction(named: fnName) else {
            throw NSError(
                domain: "BlickliCoreAI",
                code: 1,
                userInfo: [NSLocalizedDescriptionKey: "model has no '\(fnName)' function"]
            )
        }

        let desc = fn.descriptor
        guard let inputName = desc.inputNames.first else {
            throw NSError(
                domain: "BlickliCoreAI",
                code: 2,
                userInfo: [NSLocalizedDescriptionKey: "model has no inputs"]
            )
        }
        guard let outputName = desc.outputNames.first else {
            throw NSError(
                domain: "BlickliCoreAI",
                code: 3,
                userInfo: [NSLocalizedDescriptionKey: "model has no outputs"]
            )
        }

        guard case .ndArray(let inDesc) = desc.inputDescriptor(of: inputName) else {
            throw NSError(
                domain: "BlickliCoreAI",
                code: 4,
                userInfo: [NSLocalizedDescriptionKey: "Core AI model input '\(inputName)' is not an NDArray"]
            )
        }
        guard case .ndArray(let outDesc) = desc.outputDescriptor(of: outputName) else {
            throw NSError(
                domain: "BlickliCoreAI",
                code: 5,
                userInfo: [NSLocalizedDescriptionKey: "Core AI model output '\(outputName)' is not an NDArray"]
            )
        }

        guard let (inLayout, inH, inW) = parseTensorShape(inDesc.shape), inW > 0, inH > 0 else {
            throw NSError(
                domain: "BlickliCoreAI",
                code: 6,
                userInfo: [NSLocalizedDescriptionKey: "unsupported Core AI input shape \(inDesc.shape)"]
            )
        }
        guard let (_, outH, outW) = parseTensorShape(outDesc.shape), outW >= inW, outH >= inH,
              outW % inW == 0, outH % inH == 0, outW / inW == outH / inH else {
            throw NSError(
                domain: "BlickliCoreAI",
                code: 7,
                userInfo: [NSLocalizedDescriptionKey: "unsupported Core AI output shape \(outDesc.shape) for input \(inDesc.shape)"]
            )
        }

        let scalar: ScalarKind
        switch inDesc.scalarType {
        case .float16:
            scalar = .float16
        case .float32:
            scalar = .float32
        default:
            throw NSError(
                domain: "BlickliCoreAI",
                code: 8,
                userInfo: [NSLocalizedDescriptionKey: "unsupported Core AI scalar type \(inDesc.scalarType)"]
            )
        }

        return CoreAiModelBox(
            model: model,
            function: fn,
            inputName: inputName,
            outputName: outputName,
            inputShape: inDesc.shape,
            inputLayout: inLayout,
            inputScalar: scalar,
            tileWidth: inW,
            tileHeight: inH,
            outputWidth: outW,
            outputHeight: outH,
            scale: outW / inW
        )
    }

    switch loaded {
    case .success(let box):
        outTileW.pointee = box.tileWidth
        outTileH.pointee = box.tileHeight
        outScale.pointee = box.scale
        return Unmanaged.passRetained(box).toOpaque()
    case .failure(let error):
        writeError(error.localizedDescription, out: outErr)
        return nil
    }
}

@_cdecl("blickli_coreai_free")
public func blickli_coreai_free(_ handle: UnsafeMutableRawPointer?) {
    guard let handle else { return }
    Unmanaged<CoreAiModelBox>.fromOpaque(handle).release()
}

private func buildInputBuffer<S>(
    rgb: UnsafePointer<UInt8>,
    pixelCount: Int,
    layout: TensorLayout,
    lut: [S]
) -> [S] {
    let total = pixelCount * 3
    return [S](unsafeUninitializedCapacity: total) { buf, initializedCount in
        guard let base = buf.baseAddress else {
            initializedCount = 0
            return
        }
        switch layout {
        case .nchw, .chw:
            let gPlane = base.advanced(by: pixelCount)
            let bPlane = base.advanced(by: 2 * pixelCount)
            for i in 0..<pixelCount {
                let s = i * 3
                base[i] = lut[Int(rgb[s])]
                gPlane[i] = lut[Int(rgb[s + 1])]
                bPlane[i] = lut[Int(rgb[s + 2])]
            }
        case .nhwc, .hwc:
            for i in 0..<total {
                base[i] = lut[Int(rgb[i])]
            }
        }
        initializedCount = total
    }
}

private func buildInputNDArray(
    rgb: UnsafePointer<UInt8>,
    width: Int,
    height: Int,
    shape: [Int],
    layout: TensorLayout,
    scalar: ScalarKind
) -> NDArray {
    let pixelCount = width * height
    switch scalar {
    case .float16:
        let data = buildInputBuffer(
            rgb: rgb, pixelCount: pixelCount, layout: layout, lut: u8ToF16
        )
        return NDArray(scalars: data, shape: shape)
    case .float32:
        let data = buildInputBuffer(
            rgb: rgb, pixelCount: pixelCount, layout: layout, lut: u8ToF32
        )
        return NDArray(scalars: data, shape: shape)
    }
}

private func sanitize(_ v: Float) -> Float {
    v.isFinite ? v : 0.0
}

private func copyStridedOutput<T>(
    ptr: UnsafePointer<T>,
    strides: Span<Int>,
    layout: TensorLayout,
    width: Int,
    height: Int,
    outRgb: UnsafeMutablePointer<Float>,
    toFloat: (T) -> Float
) -> Float {
    let sc: Int
    let sy: Int
    let sx: Int
    switch layout {
    case .nchw: (sc, sy, sx) = (strides[1], strides[2], strides[3])
    case .nhwc: (sc, sy, sx) = (strides[3], strides[1], strides[2])
    case .chw:  (sc, sy, sx) = (strides[0], strides[1], strides[2])
    case .hwc:  (sc, sy, sx) = (strides[2], strides[0], strides[1])
    }
    var maxSeen: Float = 0.0
    for y in 0..<height {
        let rowOff = y * sy
        let dstRow = y * width
        for x in 0..<width {
            let base = rowOff + x * sx
            let r = sanitize(toFloat(ptr[base]))
            let g = sanitize(toFloat(ptr[base + sc]))
            let b = sanitize(toFloat(ptr[base + 2 * sc]))
            if r > maxSeen { maxSeen = r }
            if g > maxSeen { maxSeen = g }
            if b > maxSeen { maxSeen = b }
            let dst = (dstRow + x) * 3
            outRgb[dst] = r
            outRgb[dst + 1] = g
            outRgb[dst + 2] = b
        }
    }
    return maxSeen
}

private func readOutputNDArray(
    _ array: NDArray,
    outRgb: UnsafeMutablePointer<Float>,
    expectedW: Int,
    expectedH: Int
) throws -> Float {
    guard let (layout, height, width) = parseTensorShape(array.shape),
          width == expectedW, height == expectedH else {
        throw NSError(
            domain: "BlickliCoreAI",
            code: 10,
            userInfo: [NSLocalizedDescriptionKey: "unexpected Core AI output shape \(array.shape)"]
        )
    }

    switch array.scalarType {
    case .float16:
        let view = array.view(as: Float16.self)
        return view.withUnsafePointer { ptr, _, strides in
            copyStridedOutput(
                ptr: ptr, strides: strides, layout: layout,
                width: width, height: height, outRgb: outRgb
            ) { Float($0) }
        }
    case .float32:
        let view = array.view(as: Float.self)
        return view.withUnsafePointer { ptr, _, strides in
            copyStridedOutput(
                ptr: ptr, strides: strides, layout: layout,
                width: width, height: height, outRgb: outRgb
            ) { $0 }
        }
    default:
        throw NSError(
            domain: "BlickliCoreAI",
            code: 11,
            userInfo: [NSLocalizedDescriptionKey: "unsupported Core AI output scalar type \(array.scalarType)"]
        )
    }
}

@_cdecl("blickli_coreai_predict")
public func blickli_coreai_predict(
    _ handle: UnsafeMutableRawPointer,
    _ inRgb: UnsafePointer<UInt8>,
    _ outRgb: UnsafeMutablePointer<Float>,
    _ outMax: UnsafeMutablePointer<Float>?,
    _ outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Bool {
    let box = Unmanaged<CoreAiModelBox>.fromOpaque(handle).takeUnretainedValue()
    let input = buildInputNDArray(
        rgb: inRgb,
        width: box.tileWidth,
        height: box.tileHeight,
        shape: box.inputShape,
        layout: box.inputLayout,
        scalar: box.inputScalar
    )

    let predicted = runBlocking { () async throws -> NDArray in
        var outputs = try await box.function.run(inputs: [box.inputName: input])
        guard let value = outputs.remove(box.outputName), let outArray = value.ndArray else {
            throw NSError(
                domain: "BlickliCoreAI",
                code: 9,
                userInfo: [NSLocalizedDescriptionKey: "Core AI model returned no output '\(box.outputName)'"]
            )
        }
        return outArray
    }

    switch predicted {
    case .success(let outArray):
        do {
            let maxSeen = try readOutputNDArray(
                outArray,
                outRgb: outRgb,
                expectedW: box.outputWidth,
                expectedH: box.outputHeight
            )
            outMax?.pointee = maxSeen
            return true
        } catch {
            writeError(error.localizedDescription, out: outErr)
            return false
        }
    case .failure(let error):
        writeError(error.localizedDescription, out: outErr)
        return false
    }
}
