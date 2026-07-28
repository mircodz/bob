import SwiftUI

/// Design system for Bob — option 1b "Native", a light, grouped-iOS look that
/// reads like Messages rather than a terminal. Tokens are lifted directly from
/// the design handoff (colors, type scale, radii, spacing, shadows).
///
/// Convention: SF (system) for chrome and prose; monospaced for anything the
/// agent typed — paths, commands, diffs, counts.
enum Theme {

    // MARK: Surfaces
    static let bg      = Color(hex: 0xF6F5F2)  // app background
    static let canvas  = Color(hex: 0xF2F1ED)  // alt canvas (code fill)
    static let surface = Color.white           // cards

    // MARK: Text
    static let text      = Color(hex: 0x16161A)
    static let secondary = Color(hex: 0x16161A, alpha: 0.5)
    static let tertiary  = Color(hex: 0x16161A, alpha: 0.35)

    // MARK: Lines / fills
    static let hairline    = Color(hex: 0x000000, alpha: 0.07)

    // MARK: Accent
    static let accent = Color(hex: 0x2A78D6)   // user bubble + links

    // MARK: Semantic (hue carries meaning; from oklch → sRGB)
    // Tool family: read / search (blue).
    static let toolBadgeBg = Color(rgb: 0.416, 0.655, 0.958, 0.14) // 0.72 .13 255
    static let toolBadgeFg = Color(rgb: 0.215, 0.449, 0.735)       // 0.55 .13 255
    // Shell / test (red).
    static let shellBadgeBg = Color(rgb: 0.921, 0.510, 0.481, 0.14) // 0.72 .13 25
    static let shellBadgeFg = Color(rgb: 0.659, 0.211, 0.206)       // 0.5 .15 25
    // Subagent (purple).
    static let subBadgeBg = Color(rgb: 0.695, 0.568, 0.916, 0.14)   // 0.72 .13 300
    static let subBadgeFg = Color(rgb: 0.437, 0.309, 0.633)         // 0.5 .13 300
    // Diff / counts.
    static let added   = Color(rgb: 0.161, 0.525, 0.276)  // 0.55 .13 150
    static let removed = Color(rgb: 0.659, 0.211, 0.206)  // 0.5 .15 25
    static let attention = Color(rgb: 0.769, 0.628, 0.197) // 0.72 .13 90 (running)

    // Amber — inline agent question (7a).
    static let amber        = Color(rgb: 0.769, 0.628, 0.197) // 0.72 .13 90 (border)
    static let amberText    = Color(rgb: 0.435, 0.257, 0.0)   // 0.42 .11 75 (label)
    static let amberBadgeBg = Color(rgb: 0.869, 0.631, 0.263) // 0.75 .13 75
    static let amberSubtitle = Color(rgb: 0.47, 0.29, 0.0)    // 0.45 .11 75 (header subtitle)

    // MARK: Type
    static func sf(_ size: CGFloat, _ weight: Font.Weight = .regular) -> Font {
        .system(size: size, weight: weight)
    }
    static func mono(_ size: CGFloat, _ weight: Font.Weight = .regular) -> Font {
        .system(size: size, weight: weight, design: .monospaced)
    }

    // MARK: Spacing
    static let s1: CGFloat = 4
    static let s2: CGFloat = 8
    static let s2h: CGFloat = 10
    static let s3: CGFloat = 12
    static let s3h: CGFloat = 14
    static let s4: CGFloat = 16
    static let s6: CGFloat = 24

    // MARK: Radius
    static let r8: CGFloat = 8
    static let r10: CGFloat = 10
    static let r12: CGFloat = 12
    static let r14: CGFloat = 14
    static let r18: CGFloat = 18
}


/// Card shadow from the handoff: `0 1px 2px rgba(0,0,0,0.05)`.
extension View {
    func cardShadow() -> some View {
        self.shadow(color: .black.opacity(0.05), radius: 1, x: 0, y: 1)
    }
}

extension Color {
    init(hex: UInt32, alpha: Double = 1) {
        self.init(
            .sRGB,
            red: Double((hex >> 16) & 0xFF) / 255,
            green: Double((hex >> 8) & 0xFF) / 255,
            blue: Double(hex & 0xFF) / 255,
            opacity: alpha)
    }
    init(rgb r: Double, _ g: Double, _ b: Double, _ a: Double = 1) {
        self.init(.sRGB, red: r, green: g, blue: b, opacity: a)
    }
}
