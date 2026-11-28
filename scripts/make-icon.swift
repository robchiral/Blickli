// Render the Blickli app icon at all sizes required by AppIcon.appiconset.
//
//   swift scripts/make-icon.swift [folder]   # default branding/macos/AppIcon.appiconset
//
// Drawn in a 1024x1024 coordinate space and scaled per target size.

import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let srgb = CGColorSpace(name: CGColorSpace.sRGB)!

func rgba(_ hex: UInt32, _ alpha: CGFloat = 1) -> CGColor {
    CGColor(
        srgbRed: CGFloat((hex >> 16) & 0xFF) / 255,
        green: CGFloat((hex >> 8) & 0xFF) / 255,
        blue: CGFloat(hex & 0xFF) / 255,
        alpha: alpha)
}

func gradient(_ stops: [(CGFloat, UInt32, CGFloat)]) -> CGGradient {
    CGGradient(
        colorsSpace: srgb,
        colors: stops.map { rgba($0.1, $0.2) } as CFArray,
        locations: stops.map { $0.0 })!
}

/// Continuous rounded rectangle matching macOS SwiftUI RoundedRectangle(style: .continuous).
func continuousRoundedRect(_ rect: CGRect, radius rawRadius: CGFloat) -> CGPath {
    let maxR = min(rect.width, rect.height) / (2 * 1.52866495)
    let r = max(0.5, min(rawRadius, maxR))
    let p = CGMutablePath()

    let x0 = rect.minX, x1 = rect.maxX
    let y0 = rect.minY, y1 = rect.maxY

    let k: CGFloat = 1.52866495 * r
    let c1: CGFloat = 1.08849001 * r
    let c2: CGFloat = 0.86840701 * r
    let p1x: CGFloat = 0.63149399 * r
    let p1y: CGFloat = 0.07491140 * r
    let c3: CGFloat = 0.37282401 * r
    let c4: CGFloat = 0.16906001 * r

    p.move(to: CGPoint(x: x1, y: (y0 + y1) * 0.5))

    // Top-right corner
    p.addLine(to: CGPoint(x: x1, y: y1 - k))
    p.addCurve(to: CGPoint(x: x1 - p1y, y: y1 - p1x),
               control1: CGPoint(x: x1, y: y1 - c1),
               control2: CGPoint(x: x1, y: y1 - c2))
    p.addCurve(to: CGPoint(x: x1 - p1x, y: y1 - p1y),
               control1: CGPoint(x: x1 - c4, y: y1 - c3),
               control2: CGPoint(x: x1 - c3, y: y1 - c4))
    p.addCurve(to: CGPoint(x: x1 - k, y: y1),
               control1: CGPoint(x: x1 - c2, y: y1),
               control2: CGPoint(x: x1 - c1, y: y1))

    // Top-left corner
    p.addLine(to: CGPoint(x: x0 + k, y: y1))
    p.addCurve(to: CGPoint(x: x0 + p1x, y: y1 - p1y),
               control1: CGPoint(x: x0 + c1, y: y1),
               control2: CGPoint(x: x0 + c2, y: y1))
    p.addCurve(to: CGPoint(x: x0 + p1y, y: y1 - p1x),
               control1: CGPoint(x: x0 + c3, y: y1 - c4),
               control2: CGPoint(x: x0 + c4, y: y1 - c3))
    p.addCurve(to: CGPoint(x: x0, y: y1 - k),
               control1: CGPoint(x: x0, y: y1 - c2),
               control2: CGPoint(x: x0, y: y1 - c1))

    // Bottom-left corner
    p.addLine(to: CGPoint(x: x0, y: y0 + k))
    p.addCurve(to: CGPoint(x: x0 + p1y, y: y0 + p1x),
               control1: CGPoint(x: x0, y: y0 + c1),
               control2: CGPoint(x: x0, y: y0 + c2))
    p.addCurve(to: CGPoint(x: x0 + p1x, y: y0 + p1y),
               control1: CGPoint(x: x0 + c4, y: y0 + c3),
               control2: CGPoint(x: x0 + c3, y: y0 + c4))
    p.addCurve(to: CGPoint(x: x0 + k, y: y0),
               control1: CGPoint(x: x0 + c2, y: y0),
               control2: CGPoint(x: x0 + c1, y: y0))

    // Bottom-right corner
    p.addLine(to: CGPoint(x: x1 - k, y: y0))
    p.addCurve(to: CGPoint(x: x1 - p1x, y: y0 + p1y),
               control1: CGPoint(x: x1 - c1, y: y0),
               control2: CGPoint(x: x1 - c2, y: y0))
    p.addCurve(to: CGPoint(x: x1 - p1y, y: y0 + p1x),
               control1: CGPoint(x: x1 - c3, y: y0 + c4),
               control2: CGPoint(x: x1 - c4, y: y0 + c3))
    p.addCurve(to: CGPoint(x: x1, y: y0 + k),
               control1: CGPoint(x: x1, y: y0 + c2),
               control2: CGPoint(x: x1, y: y0 + c1))

    p.closeSubpath()
    return p
}

/// Draw an inner specular glass rim and bottom thickness shade inside an active clip.
func drawInnerGlassRim(in ctx: CGContext, rect: CGRect, radius: CGFloat,
                       topAlpha: CGFloat, bottomAlpha: CGFloat,
                       width: CGFloat = 5, bottomShadowAlpha: CGFloat = 0.06) {
    if bottomShadowAlpha > 0 {
        ctx.saveGState()
        let shadowRect = rect.insetBy(dx: 3, dy: 3)
        ctx.addPath(continuousRoundedRect(shadowRect, radius: max(1, radius - 3)))
        ctx.setLineWidth(6)
        ctx.replacePathWithStrokedPath()
        ctx.clip()
        ctx.drawLinearGradient(
            gradient([
                (0.0, 0x0A1A33, 0.0),
                (0.76, 0x0A1A33, 0.0),
                (1.0, 0x0A1A33, bottomShadowAlpha)
            ]),
            start: CGPoint(x: rect.midX, y: rect.maxY),
            end: CGPoint(x: rect.midX, y: rect.minY),
            options: [])
        ctx.restoreGState()
    }

    ctx.saveGState()
    let insetRect = rect.insetBy(dx: width * 0.5, dy: width * 0.5)
    ctx.addPath(continuousRoundedRect(insetRect, radius: max(1, radius - width * 0.5)))
    ctx.setLineWidth(width)
    ctx.replacePathWithStrokedPath()
    ctx.clip()
    ctx.drawLinearGradient(
        gradient([
            (0.0, 0xFFFFFF, topAlpha),
            (0.25, 0xFFFFFF, topAlpha * 0.42),
            (0.52, 0xFFFFFF, 0.06),
            (0.80, 0xFFFFFF, bottomAlpha * 0.55),
            (1.0, 0xFFFFFF, bottomAlpha)
        ]),
        start: CGPoint(x: rect.minX, y: rect.maxY),
        end: CGPoint(x: rect.maxX, y: rect.minY),
        options: [])
    ctx.restoreGState()

    ctx.saveGState()
    let topRect = rect.insetBy(dx: 1.5, dy: 1.5)
    ctx.addPath(continuousRoundedRect(topRect, radius: max(1, radius - 1.5)))
    ctx.setLineWidth(3)
    ctx.replacePathWithStrokedPath()
    ctx.clip()
    ctx.drawLinearGradient(
        gradient([(0.0, 0xFFFFFF, topAlpha * 0.9), (0.16, 0xFFFFFF, 0.0)]),
        start: CGPoint(x: rect.midX, y: rect.maxY),
        end: CGPoint(x: rect.midX, y: rect.minY),
        options: [])
    ctx.restoreGState()
}

func drawWarmSunsetPhoto(in ctx: CGContext, rect: CGRect) {
    let x = rect.minX, y = rect.minY, w = rect.width, h = rect.height
    ctx.saveGState()

    ctx.drawLinearGradient(
        gradient([
            (0.0, 0xFF5476, 1.0),
            (0.40, 0xFF8B58, 1.0),
            (0.76, 0xFFC56B, 1.0),
            (1.0, 0xFFE9A8, 1.0)
        ]),
        start: CGPoint(x: rect.minX, y: rect.maxY),
        end: CGPoint(x: rect.maxX, y: rect.minY),
        options: [])

    ctx.drawRadialGradient(
        gradient([(0.0, 0xFFF8DB, 0.72), (0.50, 0xFFD282, 0.24), (1.0, 0xFFD282, 0.0)]),
        startCenter: CGPoint(x: x + w * 0.24, y: y + h * 0.74), startRadius: 0,
        endCenter: CGPoint(x: x + w * 0.24, y: y + h * 0.74), endRadius: w * 0.46,
        options: [])

    let hill = CGMutablePath()
    hill.move(to: CGPoint(x: x - 10, y: y - 10))
    hill.addLine(to: CGPoint(x: x - 10, y: y + h * 0.44))
    hill.addCurve(to: CGPoint(x: x + w + 10, y: y + h * 0.30),
                  control1: CGPoint(x: x + w * 0.32, y: y + h * 0.56),
                  control2: CGPoint(x: x + w * 0.68, y: y + h * 0.26))
    hill.addLine(to: CGPoint(x: x + w + 10, y: y - 10))
    hill.closeSubpath()
    ctx.addPath(hill)
    ctx.setFillColor(rgba(0x7A2D5E, 0.48))
    ctx.fillPath()

    ctx.restoreGState()
}

func drawAlpinePhoto(in ctx: CGContext, rect: CGRect, cornerRadius: CGFloat) {
    let x = rect.minX, y = rect.minY, w = rect.width, h = rect.height
    ctx.saveGState()

    // Sky gradient
    ctx.drawLinearGradient(
        gradient([
            (0.0, 0x0E72FA, 1.0),
            (0.36, 0x38ADFF, 1.0),
            (0.68, 0x8EDBFF, 1.0),
            (1.0, 0xDCF5FF, 1.0)
        ]),
        start: CGPoint(x: rect.midX, y: rect.maxY),
        end: CGPoint(x: rect.midX, y: y + h * 0.18),
        options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])

    // Sun and atmospheric halo
    let sunC = CGPoint(x: x + w * 0.24, y: y + h * 0.72)
    ctx.drawRadialGradient(
        gradient([
            (0.0, 0xFFFCEB, 0.78),
            (0.26, 0xFFE699, 0.36),
            (0.60, 0xFFD980, 0.11),
            (1.0, 0xFFD980, 0.0)
        ]),
        startCenter: sunC, startRadius: 0,
        endCenter: sunC, endRadius: w * 0.36,
        options: [])

    let sunR = w * 0.082
    ctx.saveGState()
    ctx.addEllipse(in: CGRect(x: sunC.x - sunR, y: sunC.y - sunR, width: sunR * 2, height: sunR * 2))
    ctx.clip()
    ctx.drawLinearGradient(
        gradient([(0.0, 0xFFFDF5, 1.0), (0.55, 0xFFE791, 1.0), (1.0, 0xFFCB4D, 1.0)]),
        start: CGPoint(x: sunC.x - sunR * 0.3, y: sunC.y + sunR),
        end: CGPoint(x: sunC.x + sunR * 0.3, y: sunC.y - sunR),
        options: [])
    ctx.restoreGState()

    // Far misty ridge
    let farMtn = CGMutablePath()
    farMtn.move(to: CGPoint(x: x - 12, y: y - 12))
    farMtn.addLine(to: CGPoint(x: x - 12, y: y + h * 0.38))
    farMtn.addCurve(to: CGPoint(x: x + w * 0.22, y: y + h * 0.50),
                    control1: CGPoint(x: x + w * 0.06, y: y + h * 0.44),
                    control2: CGPoint(x: x + w * 0.14, y: y + h * 0.52))
    farMtn.addCurve(to: CGPoint(x: x + w * 0.46, y: y + h * 0.38),
                    control1: CGPoint(x: x + w * 0.30, y: y + h * 0.48),
                    control2: CGPoint(x: x + w * 0.38, y: y + h * 0.40))
    farMtn.addCurve(to: CGPoint(x: x + w * 0.84, y: y + h * 0.52),
                    control1: CGPoint(x: x + w * 0.62, y: y + h * 0.42),
                    control2: CGPoint(x: x + w * 0.74, y: y + h * 0.56))
    farMtn.addCurve(to: CGPoint(x: x + w + 12, y: y + h * 0.44),
                    control1: CGPoint(x: x + w * 0.92, y: y + h * 0.49),
                    control2: CGPoint(x: x + w * 0.97, y: y + h * 0.46))
    farMtn.addLine(to: CGPoint(x: x + w + 12, y: y - 12))
    farMtn.closeSubpath()

    ctx.saveGState()
    ctx.addPath(farMtn)
    ctx.clip()
    ctx.drawLinearGradient(
        gradient([(0.0, 0x6AA6EA, 0.92), (0.65, 0x99CBF8, 0.78), (1.0, 0xD2EEFF, 0.55)]),
        start: CGPoint(x: rect.midX, y: y + h * 0.55),
        end: CGPoint(x: rect.midX, y: y + h * 0.20),
        options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
    ctx.restoreGState()

    // Soft horizon mist behind the main alpine peak
    ctx.drawLinearGradient(
        gradient([(0.0, 0xCFEFFF, 0.0), (0.65, 0xCFEFFF, 0.32), (1.0, 0xE8F8FF, 0.45)]),
        start: CGPoint(x: rect.midX, y: y + h * 0.48),
        end: CGPoint(x: rect.midX, y: y + h * 0.14),
        options: [])

    // Hero alpine peak with sunlit left face and shaded right face
    let peakX = x + w * 0.60
    let peakY = y + h * 0.66
    let mainMtn = CGMutablePath()
    mainMtn.move(to: CGPoint(x: x - 12, y: y - 12))
    mainMtn.addLine(to: CGPoint(x: x + w * 0.08, y: y + h * 0.16))
    mainMtn.addCurve(to: CGPoint(x: peakX - w * 0.032, y: peakY - h * 0.014),
                     control1: CGPoint(x: x + w * 0.28, y: y + h * 0.34),
                     control2: CGPoint(x: x + w * 0.46, y: y + h * 0.56))
    mainMtn.addQuadCurve(to: CGPoint(x: peakX + w * 0.032, y: peakY - h * 0.016),
                         control: CGPoint(x: peakX, y: peakY + h * 0.014))
    mainMtn.addCurve(to: CGPoint(x: x + w + 12, y: y + h * 0.22),
                     control1: CGPoint(x: x + w * 0.74, y: y + h * 0.54),
                     control2: CGPoint(x: x + w * 0.88, y: y + h * 0.32))
    mainMtn.addLine(to: CGPoint(x: x + w + 12, y: y - 12))
    mainMtn.closeSubpath()

    ctx.saveGState()
    ctx.addPath(mainMtn)
    ctx.clip()
    ctx.drawLinearGradient(
        gradient([
            (0.0, 0x5E9FF2, 1.0),
            (0.45, 0x356EC9, 1.0),
            (1.0, 0x1E4690, 1.0)
        ]),
        start: CGPoint(x: peakX - w * 0.16, y: peakY),
        end: CGPoint(x: peakX + w * 0.04, y: y + h * 0.10),
        options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])

    // Natural curved ridge spine separating sunlit and shaded faces
    let shadedFacet = CGMutablePath()
    shadedFacet.move(to: CGPoint(x: peakX + w * 0.004, y: peakY + h * 0.02))
    shadedFacet.addCurve(to: CGPoint(x: peakX - w * 0.015, y: y + h * 0.38),
                         control1: CGPoint(x: peakX + w * 0.02, y: y + h * 0.54),
                         control2: CGPoint(x: peakX - w * 0.03, y: y + h * 0.46))
    shadedFacet.addCurve(to: CGPoint(x: peakX + w * 0.14, y: y - 12),
                         control1: CGPoint(x: peakX + w * 0.01, y: y + h * 0.26),
                         control2: CGPoint(x: peakX + w * 0.08, y: y + h * 0.10))
    shadedFacet.addLine(to: CGPoint(x: x + w + 16, y: y - 12))
    shadedFacet.addLine(to: CGPoint(x: x + w + 16, y: peakY + h * 0.02))
    shadedFacet.closeSubpath()

    ctx.saveGState()
    ctx.addPath(shadedFacet)
    ctx.clip()
    ctx.drawLinearGradient(
        gradient([
            (0.0, 0x264D99, 1.0),
            (0.55, 0x193773, 1.0),
            (1.0, 0x102350, 1.0)
        ]),
        start: CGPoint(x: peakX, y: peakY),
        end: CGPoint(x: x + w * 0.92, y: y + h * 0.06),
        options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
    ctx.restoreGState()

    // Soft sunlit highlight along the upper-left crest
    ctx.drawRadialGradient(
        gradient([(0.0, 0xEAF6FF, 0.38), (0.55, 0xB8E0FF, 0.12), (1.0, 0xB8E0FF, 0.0)]),
        startCenter: CGPoint(x: peakX - w * 0.04, y: peakY - h * 0.02), startRadius: 0,
        endCenter: CGPoint(x: peakX - w * 0.04, y: peakY - h * 0.02), endRadius: w * 0.22,
        options: [])
    ctx.restoreGState()

    // Foreground left ridge anchoring the composition
    let forePeakX = x + w * 0.24
    let forePeakY = y + h * 0.44
    let foreMtn = CGMutablePath()
    foreMtn.move(to: CGPoint(x: x - 12, y: y - 12))
    foreMtn.addLine(to: CGPoint(x: x - 12, y: y + h * 0.30))
    foreMtn.addCurve(to: CGPoint(x: forePeakX - w * 0.025, y: forePeakY - h * 0.008),
                     control1: CGPoint(x: x + w * 0.06, y: y + h * 0.36),
                     control2: CGPoint(x: x + w * 0.16, y: y + h * 0.43))
    foreMtn.addQuadCurve(to: CGPoint(x: forePeakX + w * 0.032, y: forePeakY - h * 0.012),
                         control: CGPoint(x: forePeakX, y: forePeakY + h * 0.010))
    foreMtn.addCurve(to: CGPoint(x: x + w * 0.74, y: y + h * 0.10),
                     control1: CGPoint(x: x + w * 0.40, y: y + h * 0.36),
                     control2: CGPoint(x: x + w * 0.56, y: y + h * 0.16))
    foreMtn.addCurve(to: CGPoint(x: x + w + 12, y: y + h * 0.15),
                     control1: CGPoint(x: x + w * 0.86, y: y + h * 0.07),
                     control2: CGPoint(x: x + w * 0.94, y: y + h * 0.12))
    foreMtn.addLine(to: CGPoint(x: x + w + 12, y: y - 12))
    foreMtn.closeSubpath()

    ctx.saveGState()
    ctx.addPath(foreMtn)
    ctx.clip()
    ctx.drawLinearGradient(
        gradient([
            (0.0, 0x254D99, 1.0),
            (0.45, 0x163169, 1.0),
            (1.0, 0x0B1B40, 1.0)
        ]),
        start: CGPoint(x: forePeakX - w * 0.12, y: forePeakY),
        end: CGPoint(x: x + w * 0.55, y: y),
        options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])

    let foreShade = CGMutablePath()
    foreShade.move(to: CGPoint(x: forePeakX, y: forePeakY + h * 0.01))
    foreShade.addCurve(to: CGPoint(x: forePeakX + w * 0.12, y: y - 12),
                       control1: CGPoint(x: forePeakX + w * 0.05, y: y + h * 0.28),
                       control2: CGPoint(x: forePeakX + w * 0.08, y: y + h * 0.12))
    foreShade.addLine(to: CGPoint(x: x + w + 12, y: y - 12))
    foreShade.addLine(to: CGPoint(x: x + w + 12, y: forePeakY))
    foreShade.closeSubpath()
    ctx.addPath(foreShade)
    ctx.setFillColor(rgba(0x0A1838, 0.42))
    ctx.fillPath()
    ctx.restoreGState()

    // Subtle recessed inner frame edge around the picture window
    ctx.addPath(continuousRoundedRect(rect, radius: cornerRadius))
    ctx.setStrokeColor(rgba(0x001233, 0.14))
    ctx.setLineWidth(3)
    ctx.strokePath()

    // Delicate diagonal gloss reflection across the front photo
    ctx.drawLinearGradient(
        gradient([
            (0.0, 0xFFFFFF, 0.18),
            (0.40, 0xFFFFFF, 0.04),
            (0.44, 0xFFFFFF, 0.0)
        ]),
        start: CGPoint(x: rect.minX, y: rect.maxY),
        end: CGPoint(x: rect.maxX, y: rect.minY),
        options: [])

    ctx.restoreGState()
}

func drawIcon(in ctx: CGContext, pixels: Int) {
    let small = pixels < 64
    let tileRect = CGRect(x: 100, y: 100, width: 824, height: 824)
    let tileR: CGFloat = 185.4
    let tilePath = continuousRoundedRect(tileRect, radius: tileR)

    // Soft macOS ground shadow using an inset light caster to avoid dark edge bleed
    let shadowPath = continuousRoundedRect(tileRect.insetBy(dx: 2, dy: 2), radius: tileR - 2)
    if !small {
        ctx.saveGState()
        ctx.setShadow(offset: CGSize(width: 0, height: -14), blur: 28, color: rgba(0x000000, 0.25))
        ctx.addPath(shadowPath)
        ctx.setFillColor(rgba(0xECEFF5, 1.0))
        ctx.fillPath()
        ctx.restoreGState()

        ctx.saveGState()
        ctx.setShadow(offset: CGSize(width: 0, height: -3), blur: 6, color: rgba(0x000000, 0.12))
        ctx.addPath(shadowPath)
        ctx.setFillColor(rgba(0xECEFF5, 1.0))
        ctx.fillPath()
        ctx.restoreGState()
    } else {
        let px = 1024 / CGFloat(pixels)
        ctx.saveGState()
        ctx.setShadow(offset: CGSize(width: 0, height: -0.5 * px), blur: 1.0 * px, color: rgba(0x000000, 0.20))
        ctx.addPath(shadowPath)
        ctx.setFillColor(rgba(0xECEFF5, 1.0))
        ctx.fillPath()
        ctx.restoreGState()
    }

    ctx.saveGState()
    ctx.addPath(tilePath)
    ctx.clip()

    // Luminous satin porcelain background tile (Photos / Preview / Calendar family)
    ctx.drawLinearGradient(
        gradient([
            (0.0, 0xFFFFFF, 1.0),
            (0.55, 0xF5F7FB, 1.0),
            (1.0, 0xE2E8F1, 1.0)
        ]),
        start: CGPoint(x: 512, y: 924),
        end: CGPoint(x: 512, y: 100),
        options: [])
    ctx.drawRadialGradient(
        gradient([(0.0, 0xFFFFFF, 0.95), (1.0, 0xFFFFFF, 0.0)]),
        startCenter: CGPoint(x: 420, y: 880), startRadius: 0,
        endCenter: CGPoint(x: 420, y: 880), endRadius: 560,
        options: [])

    // Slight optical upscaling at 16px/32px so the stacked photos stay legible in Finder list view
    ctx.saveGState()
    if small {
        ctx.translateBy(x: 512, y: 512)
        ctx.scaleBy(x: 1.07, y: 1.07)
        ctx.translateBy(x: -512, y: -512)
    }

    // Back photo card: warm golden-hour sunset
    ctx.saveGState()
    ctx.translateBy(x: 470, y: 556)
    ctx.rotate(by: 11 * .pi / 180)
    let backRect = CGRect(x: -246, y: -194, width: 492, height: 388)
    let backR: CGFloat = 62
    let backPath = continuousRoundedRect(backRect, radius: backR)

    if !small {
        ctx.saveGState()
        ctx.setShadow(offset: CGSize(width: 0, height: -14), blur: 28, color: rgba(0x3B1D33, 0.16))
        ctx.addPath(continuousRoundedRect(backRect.insetBy(dx: 1.5, dy: 1.5), radius: backR - 1.5))
        ctx.setFillColor(rgba(0xFFFFFF, 1.0))
        ctx.fillPath()
        ctx.restoreGState()
    }

    ctx.saveGState()
    ctx.addPath(backPath)
    ctx.clip()
    ctx.drawLinearGradient(
        gradient([(0.0, 0xFFFFFF, 1.0), (1.0, 0xEEF1F6, 1.0)]),
        start: CGPoint(x: 0, y: backRect.maxY), end: CGPoint(x: 0, y: backRect.minY), options: [])
    let backInset: CGFloat = small ? 16 : 22
    let backInner = backRect.insetBy(dx: backInset, dy: backInset)
    let backInnerR = backR - backInset
    let backInnerPath = continuousRoundedRect(backInner, radius: backInnerR)
    ctx.saveGState()
    ctx.addPath(backInnerPath)
    ctx.clip()
    drawWarmSunsetPhoto(in: ctx, rect: backInner)
    ctx.restoreGState()
    ctx.addPath(backInnerPath)
    ctx.setStrokeColor(rgba(0x4A1828, 0.10))
    ctx.setLineWidth(1.5)
    ctx.strokePath()
    drawInnerGlassRim(in: ctx, rect: backRect, radius: backR,
                      topAlpha: 0.90, bottomAlpha: 0.30, width: 3, bottomShadowAlpha: 0.05)
    ctx.restoreGState()

    ctx.addPath(backPath)
    ctx.setStrokeColor(rgba(0x0A1A33, 0.07))
    ctx.setLineWidth(1.2)
    ctx.strokePath()
    ctx.restoreGState()

    // Front hero photo card: sculpted alpine landscape
    ctx.saveGState()
    ctx.translateBy(x: 536, y: 484)
    ctx.rotate(by: -3.5 * .pi / 180)
    let frontRect = CGRect(x: -272, y: -216, width: 544, height: 432)
    let frontR: CGFloat = 68
    let frontPath = continuousRoundedRect(frontRect, radius: frontR)

    if !small {
        let frontShadowPath = continuousRoundedRect(frontRect.insetBy(dx: 1.5, dy: 1.5), radius: frontR - 1.5)
        ctx.saveGState()
        ctx.setShadow(offset: CGSize(width: 0, height: -24), blur: 42, color: rgba(0x0E2042, 0.28))
        ctx.addPath(frontShadowPath)
        ctx.setFillColor(rgba(0xFFFFFF, 1.0))
        ctx.fillPath()
        ctx.restoreGState()

        ctx.saveGState()
        ctx.setShadow(offset: CGSize(width: 0, height: -5), blur: 10, color: rgba(0x0E2042, 0.14))
        ctx.addPath(frontShadowPath)
        ctx.setFillColor(rgba(0xFFFFFF, 1.0))
        ctx.fillPath()
        ctx.restoreGState()
    }

    ctx.saveGState()
    ctx.addPath(frontPath)
    ctx.clip()
    ctx.drawLinearGradient(
        gradient([(0.0, 0xFFFFFF, 1.0), (1.0, 0xECF1F8, 1.0)]),
        start: CGPoint(x: 0, y: frontRect.maxY), end: CGPoint(x: 0, y: frontRect.minY), options: [])
    let frameInset: CGFloat = small ? 18 : 26
    let frontInner = frontRect.insetBy(dx: frameInset, dy: frameInset)
    let frontInnerR = frontR - frameInset
    let frontInnerPath = continuousRoundedRect(frontInner, radius: frontInnerR)
    ctx.saveGState()
    ctx.addPath(frontInnerPath)
    ctx.clip()
    drawAlpinePhoto(in: ctx, rect: frontInner, cornerRadius: frontInnerR)
    ctx.restoreGState()

    drawInnerGlassRim(in: ctx, rect: frontRect, radius: frontR,
                      topAlpha: 0.95, bottomAlpha: 0.40, width: 3.5, bottomShadowAlpha: 0.06)
    ctx.restoreGState()

    ctx.addPath(frontPath)
    ctx.setStrokeColor(rgba(0x0A1A33, 0.08))
    ctx.setLineWidth(1.2)
    ctx.strokePath()
    ctx.restoreGState()

    ctx.restoreGState() // end small optical scaling

    // Inner specular glass rim and crisp edge separation on light backgrounds
    drawInnerGlassRim(in: ctx, rect: tileRect, radius: tileR,
                      topAlpha: 0.98, bottomAlpha: 0.50, width: 5, bottomShadowAlpha: 0.07)
    let strokeWidth: CGFloat = small ? (1024 / CGFloat(pixels)) * 0.85 : 1.5
    let strokeInset = strokeWidth * 0.5
    ctx.addPath(continuousRoundedRect(tileRect.insetBy(dx: strokeInset, dy: strokeInset),
                                      radius: tileR - strokeInset))
    ctx.setStrokeColor(rgba(0x0A1A33, small ? 0.16 : 0.09))
    ctx.setLineWidth(strokeWidth)
    ctx.strokePath()

    ctx.restoreGState() // end tile clip
}

func render(pixels: Int, to url: URL) {
    let ctx = CGContext(data: nil, width: pixels, height: pixels, bitsPerComponent: 8, bytesPerRow: 0,
                        space: srgb,
                        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    ctx.scaleBy(x: CGFloat(pixels) / 1024, y: CGFloat(pixels) / 1024)
    ctx.interpolationQuality = .high
    ctx.setShouldAntialias(true)
    ctx.setAllowsAntialiasing(true)
    drawIcon(in: ctx, pixels: pixels)
    let destination = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil)!
    CGImageDestinationAddImage(destination, ctx.makeImage()!, nil)
    guard CGImageDestinationFinalize(destination) else { fatalError("could not write \(url.path)") }
}

let folder = URL(fileURLWithPath: CommandLine.arguments.dropFirst().first
                 ?? "branding/macos/AppIcon.appiconset")
for size in [16, 32, 128, 256, 512] {
    render(pixels: size, to: folder.appendingPathComponent("icon-\(size).png"))
    render(pixels: size * 2, to: folder.appendingPathComponent("icon-\(size)@2x.png"))
}
print("drew icons in \(folder.path)")
