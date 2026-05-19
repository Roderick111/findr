import Foundation
import Vision
import ImageIO
import PDFKit

struct OcrResult: Encodable {
    let path: String
    let text: String?
    let confidence: Double?
    let exif: ExifData?
    let error: String?
}

struct ExifData: Encodable {
    let date_taken: String?
    let gps: String?
}

let encoder = JSONEncoder()
encoder.outputFormatting = []

let imageExtensions: Set<String> = ["png", "jpg", "jpeg", "heic", "tiff", "webp"]
let pdfExtensions: Set<String> = ["pdf"]

let args = CommandLine.arguments.dropFirst()
guard !args.isEmpty else {
    let err = OcrResult(path: "", text: nil, confidence: nil, exif: nil, error: "Usage: findr-ocr <path1> [path2] ...")
    if let data = try? encoder.encode(err), let json = String(data: data, encoding: .utf8) {
        print(json)
    }
    exit(1)
}

for filePath in args {
    let url = URL(fileURLWithPath: filePath)
    let ext = url.pathExtension.lowercased()

    if pdfExtensions.contains(ext) {
        processPdf(path: filePath, url: url)
    } else if imageExtensions.contains(ext) {
        processImage(path: filePath, url: url)
    } else {
        outputError(path: filePath, message: "Unsupported file type: \(ext)")
    }
}

// MARK: - Image Processing

func processImage(path: String, url: URL) {
    guard let imageSource = CGImageSourceCreateWithURL(url as CFURL, nil),
          let cgImage = CGImageSourceCreateImageAtIndex(imageSource, 0, nil) else {
        outputError(path: path, message: "Could not load image")
        return
    }

    let exif = extractExif(source: imageSource)
    let (text, confidence) = ocrImage(cgImage: cgImage)
    outputResult(path: path, text: text, confidence: confidence, exif: exif)
}

// MARK: - PDF Processing

func processPdf(path: String, url: URL) {
    guard let document = PDFDocument(url: url) else {
        outputError(path: path, message: "Could not load PDF")
        return
    }

    var allText = ""
    var totalConfidence: Double = 0
    var pageCount = 0
    let maxPages = 50 // cap to avoid huge PDFs blocking

    for i in 0..<min(document.pageCount, maxPages) {
        guard let page = document.page(at: i) else { continue }

        let bounds = page.bounds(for: .mediaBox)
        // Render at 2x for better OCR quality
        let scale: CGFloat = 2.0
        let width = Int(bounds.width * scale)
        let height = Int(bounds.height * scale)

        guard let colorSpace = CGColorSpace(name: CGColorSpace.sRGB),
              let context = CGContext(
                  data: nil, width: width, height: height,
                  bitsPerComponent: 8, bytesPerRow: 0,
                  space: colorSpace,
                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
              ) else { continue }

        context.setFillColor(CGColor.white)
        context.fill(CGRect(x: 0, y: 0, width: width, height: height))
        context.scaleBy(x: scale, y: scale)

        page.draw(with: .mediaBox, to: context)

        guard let cgImage = context.makeImage() else { continue }

        let (pageText, pageConf) = ocrImage(cgImage: cgImage)
        if let t = pageText, !t.isEmpty {
            allText += t + "\n"
            totalConfidence += pageConf ?? 0
            pageCount += 1
        }
    }

    let avgConfidence = pageCount > 0 ? totalConfidence / Double(pageCount) : 0
    let finalText = allText.trimmingCharacters(in: .whitespacesAndNewlines)
    outputResult(
        path: path,
        text: finalText.isEmpty ? nil : finalText,
        confidence: finalText.isEmpty ? nil : avgConfidence,
        exif: nil
    )
}

// MARK: - Vision OCR

func ocrImage(cgImage: CGImage) -> (String?, Double?) {
    let semaphore = DispatchSemaphore(value: 0)
    var resultText: String?
    var resultConfidence: Double?

    let request = VNRecognizeTextRequest { request, error in
        defer { semaphore.signal() }
        guard error == nil,
              let observations = request.results as? [VNRecognizedTextObservation] else {
            return
        }

        var texts: [String] = []
        var confidenceSum: Double = 0
        var count = 0

        for observation in observations {
            guard observation.confidence >= 0.3,
                  let candidate = observation.topCandidates(1).first else { continue }
            texts.append(candidate.string)
            confidenceSum += Double(observation.confidence)
            count += 1
        }

        if count > 0 {
            resultText = texts.joined(separator: "\n")
            resultConfidence = confidenceSum / Double(count)
        }
    }

    request.recognitionLevel = .accurate
    request.usesLanguageCorrection = true

    let handler = VNImageRequestHandler(cgImage: cgImage, options: [:])

    DispatchQueue.global(qos: .userInitiated).async {
        try? handler.perform([request])
    }

    // 10 second timeout per image
    let timeout = semaphore.wait(timeout: .now() + 10)
    if timeout == .timedOut {
        return (nil, nil)
    }

    return (resultText, resultConfidence)
}

// MARK: - EXIF Extraction

func extractExif(source: CGImageSource) -> ExifData? {
    guard let properties = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any] else {
        return nil
    }

    var dateTaken: String?
    var gps: String?

    // EXIF date
    if let exifDict = properties[kCGImagePropertyExifDictionary] as? [CFString: Any],
       let dateStr = exifDict[kCGImagePropertyExifDateTimeOriginal] as? String {
        // Convert "2024:01:15 10:30:00" to ISO 8601
        let formatter = DateFormatter()
        formatter.dateFormat = "yyyy:MM:dd HH:mm:ss"
        if let date = formatter.date(from: dateStr) {
            let iso = ISO8601DateFormatter()
            dateTaken = iso.string(from: date)
        } else {
            dateTaken = dateStr
        }
    }

    // GPS
    if let gpsDict = properties[kCGImagePropertyGPSDictionary] as? [CFString: Any],
       let lat = gpsDict[kCGImagePropertyGPSLatitude] as? Double,
       let latRef = gpsDict[kCGImagePropertyGPSLatitudeRef] as? String,
       let lon = gpsDict[kCGImagePropertyGPSLongitude] as? Double,
       let lonRef = gpsDict[kCGImagePropertyGPSLongitudeRef] as? String {
        let latSigned = latRef == "S" ? -lat : lat
        let lonSigned = lonRef == "W" ? -lon : lon
        gps = String(format: "%.6f,%.6f", latSigned, lonSigned)
    }

    if dateTaken == nil && gps == nil { return nil }
    return ExifData(date_taken: dateTaken, gps: gps)
}

// MARK: - Output

func outputResult(path: String, text: String?, confidence: Double?, exif: ExifData?) {
    let result = OcrResult(path: path, text: text, confidence: confidence, exif: exif, error: nil)
    if let data = try? encoder.encode(result), let json = String(data: data, encoding: .utf8) {
        print(json)
    }
}

func outputError(path: String, message: String) {
    let result = OcrResult(path: path, text: nil, confidence: nil, exif: nil, error: message)
    if let data = try? encoder.encode(result), let json = String(data: data, encoding: .utf8) {
        print(json)
    }
}
