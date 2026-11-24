import Foundation

/// Third-party code the app ships, for the About screen (OL-64e). Both are built from pinned
/// source by ios/Vendor/build-libsrt.sh at these versions; a unit test checks libsrt's against the
/// linked library.
struct ThirdPartyComponent: Equatable, Identifiable {
    let name: String
    let version: String
    /// SPDX licence identifier.
    let license: String
    let source: URL
    let licenseText: URL
    let note: String

    var id: String { name }
}

enum ThirdPartyLicenses {
    static let libsrt = ThirdPartyComponent(
        name: "libsrt (Secure Reliable Transport)",
        version: "1.5.4",
        license: "MPL-2.0",
        source: URL(string: "https://github.com/Haivision/srt/tree/v1.5.4")!,
        licenseText: URL(string: "https://github.com/Haivision/srt/blob/v1.5.4/LICENSE")!,
        note: "Shipped unmodified. Its source is available at the link above."
    )

    static let mbedTLS = ThirdPartyComponent(
        name: "Mbed TLS",
        version: "3.6.8",
        license: "Apache-2.0",
        source: URL(string: "https://github.com/Mbed-TLS/mbedtls/tree/mbedtls-3.6.8")!,
        licenseText: URL(string: "https://github.com/Mbed-TLS/mbedtls/blob/mbedtls-3.6.8/LICENSE")!,
        note: "libsrt's cryptography. Dual-licensed Apache-2.0 or GPL-2.0-or-later; used under Apache-2.0."
    )

    static let all = [libsrt, mbedTLS]
}
