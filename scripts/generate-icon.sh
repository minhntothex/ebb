#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_ICNS="${1:-${ROOT_DIR}/packaging/AppIcon.icns}"
PREVIEW_PNG="${2:-}"

WORK_DIR="$(mktemp -d /tmp/ebb-icon-gen.XXXXXX)"
trap 'rm -rf "${WORK_DIR}"' EXIT

cat > "${WORK_DIR}/gen.m" <<'OBJC'
#import <Cocoa/Cocoa.h>
#import <CoreGraphics/CoreGraphics.h>
#import <ImageIO/ImageIO.h>
#import <UniformTypeIdentifiers/UniformTypeIdentifiers.h>

static CGColorRef srgb(CGColorSpaceRef cs, CGFloat r, CGFloat g, CGFloat b, CGFloat a) {
    CGFloat comps[4] = {r / 255.0, g / 255.0, b / 255.0, a};
    return CGColorCreate(cs, comps);
}

// V3 — Porcelain Light: bright Apple/Swiss squircle canvas, warm amber & emerald receding waves, dark slate anchor dot.
static CGImageRef renderIcon(int pixelSize) {
    CGColorSpaceRef cs = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
    CGContextRef ctx = CGBitmapContextCreate(
        NULL, pixelSize, pixelSize, 8, 0, cs,
        kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big
    );
    CGContextSetAllowsAntialiasing(ctx, true);
    CGContextSetShouldAntialias(ctx, true);
    CGContextSetInterpolationQuality(ctx, kCGInterpolationHigh);

    CGFloat scale = (CGFloat)pixelSize / 1024.0;
    CGContextScaleCTM(ctx, scale, scale);

    // macOS Big Sur / Sonoma icon grid: 824x824 centered squircle, r ≈ 185
    CGRect tileRect = CGRectMake(100, 100, 824, 824);
    CGPathRef tilePath = CGPathCreateWithRoundedRect(tileRect, 185, 185, NULL);

    // Soft outer macOS drop shadow
    CGContextSaveGState(ctx);
    CGColorRef shadowCol = srgb(cs, 0, 0, 0, 0.22);
    CGColorRef tileBase = srgb(cs, 245, 246, 248, 1.0);
    CGContextSetShadowWithColor(ctx, CGSizeMake(0, -14), 28, shadowCol);
    CGContextAddPath(ctx, tilePath);
    CGContextSetFillColorWithColor(ctx, tileBase);
    CGContextFillPath(ctx);
    CGColorRelease(shadowCol);
    CGColorRelease(tileBase);
    CGContextRestoreGState(ctx);

    // Clip inside rounded tile
    CGContextSaveGState(ctx);
    CGContextAddPath(ctx, tilePath);
    CGContextClip(ctx);

    // Porcelain light vertical gradient canvas
    CGFloat locs[2] = {0.0, 1.0};
    CGColorRef bgTop = srgb(cs, 252, 252, 253, 1.0);
    CGColorRef bgBot = srgb(cs, 232, 235, 241, 1.0);
    CGGradientRef bgGrad = CGGradientCreateWithColors(cs, (__bridge CFArrayRef)@[(__bridge id)bgTop, (__bridge id)bgBot], locs);
    CGContextDrawLinearGradient(ctx, bgGrad, CGPointMake(512, 924), CGPointMake(512, 100), 0);
    CGGradientRelease(bgGrad);
    CGColorRelease(bgTop);
    CGColorRelease(bgBot);

    // Upper soft amber tide curve (Claude)
    CGMutablePathRef w1 = CGPathCreateMutable();
    CGPathMoveToPoint(w1, NULL, 80, 520);
    CGPathAddCurveToPoint(w1, NULL, 320, 620, 660, 410, 960, 510);
    CGPathAddLineToPoint(w1, NULL, 960, 80);
    CGPathAddLineToPoint(w1, NULL, 80, 80);
    CGPathCloseSubpath(w1);

    CGContextSaveGState(ctx);
    CGContextAddPath(ctx, w1);
    CGContextClip(ctx);
    CGColorRef a0 = srgb(cs, 232, 134, 82, 0.92);
    CGColorRef a1 = srgb(cs, 206, 98, 48, 0.88);
    CGGradientRef ag = CGGradientCreateWithColors(cs, (__bridge CFArrayRef)@[(__bridge id)a0, (__bridge id)a1], locs);
    CGContextDrawLinearGradient(ctx, ag, CGPointMake(512, 600), CGPointMake(512, 150), 0);
    CGGradientRelease(ag);
    CGColorRelease(a0);
    CGColorRelease(a1);
    CGContextRestoreGState(ctx);
    CGPathRelease(w1);

    // Lower crisp emerald/mint receding wave (Codex)
    CGMutablePathRef w2 = CGPathCreateMutable();
    CGPathMoveToPoint(w2, NULL, 80, 355);
    CGPathAddCurveToPoint(w2, NULL, 340, 270, 620, 450, 960, 325);
    CGPathAddLineToPoint(w2, NULL, 960, 80);
    CGPathAddLineToPoint(w2, NULL, 80, 80);
    CGPathCloseSubpath(w2);

    CGContextSaveGState(ctx);
    CGContextAddPath(ctx, w2);
    CGContextClip(ctx);
    CGColorRef m0 = srgb(cs, 62, 178, 126, 0.95);
    CGColorRef m1 = srgb(cs, 24, 122, 80, 0.98);
    CGGradientRef mg = CGGradientCreateWithColors(cs, (__bridge CFArrayRef)@[(__bridge id)m0, (__bridge id)m1], locs);
    CGContextDrawLinearGradient(ctx, mg, CGPointMake(512, 430), CGPointMake(512, 100), 0);
    CGGradientRelease(mg);
    CGColorRelease(m0);
    CGColorRelease(m1);
    CGContextRestoreGState(ctx);
    CGPathRelease(w2);

    // Subtle dark slate anchor dot (top-right)
    CGColorRef dot = srgb(cs, 34, 40, 52, 0.85);
    CGContextSetFillColorWithColor(ctx, dot);
    CGContextFillEllipseInRect(ctx, CGRectMake(686, 686, 84, 84));
    CGColorRelease(dot);

    // Crisp subtle inner edge stroke
    CGColorRef rim = srgb(cs, 0, 0, 0, 0.08);
    CGContextAddPath(ctx, tilePath);
    CGContextSetStrokeColorWithColor(ctx, rim);
    CGContextSetLineWidth(ctx, 3.0);
    CGContextStrokePath(ctx);
    CGColorRelease(rim);

    CGContextRestoreGState(ctx);
    CGPathRelease(tilePath);

    CGImageRef img = CGBitmapContextCreateImage(ctx);
    CGContextRelease(ctx);
    CGColorSpaceRelease(cs);
    return img;
}

static BOOL writePng(CGImageRef img, NSString *path) {
    NSURL *url = [NSURL fileURLWithPath:path];
    CGImageDestinationRef dest = CGImageDestinationCreateWithURL(
        (__bridge CFURLRef)url,
        (__bridge CFStringRef)UTTypePNG.identifier,
        1,
        NULL
    );
    if (!dest) return NO;
    CGImageDestinationAddImage(dest, img, NULL);
    BOOL ok = CGImageDestinationFinalize(dest);
    CFRelease(dest);
    return ok;
}

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        if (argc < 2) return 2;
        NSString *iconsetDir = [NSString stringWithUTF8String:argv[1]];
        NSString *previewPath = argc >= 3 && strlen(argv[2]) > 0 ? [NSString stringWithUTF8String:argv[2]] : nil;

        struct { const char *name; int px; } specs[] = {
            {"icon_16x16.png", 16},
            {"icon_16x16@2x.png", 32},
            {"icon_32x32.png", 32},
            {"icon_32x32@2x.png", 64},
            {"icon_128x128.png", 128},
            {"icon_128x128@2x.png", 256},
            {"icon_256x256.png", 256},
            {"icon_256x256@2x.png", 512},
            {"icon_512x512.png", 512},
            {"icon_512x512@2x.png", 1024},
        };

        for (size_t i = 0; i < sizeof(specs) / sizeof(specs[0]); i++) {
            CGImageRef img = renderIcon(specs[i].px);
            NSString *dst = [iconsetDir stringByAppendingPathComponent:[NSString stringWithUTF8String:specs[i].name]];
            if (!writePng(img, dst)) {
                CGImageRelease(img);
                return 1;
            }
            if (previewPath && specs[i].px == 512 && strcmp(specs[i].name, "icon_512x512.png") == 0) {
                writePng(img, previewPath);
            }
            CGImageRelease(img);
        }
    }
    return 0;
}
OBJC

clang -fobjc-arc -framework Cocoa -framework ImageIO -framework UniformTypeIdentifiers "${WORK_DIR}/gen.m" -o "${WORK_DIR}/gen"
ICONSET_DIR="${WORK_DIR}/AppIcon.iconset"
mkdir -p "${ICONSET_DIR}"
"${WORK_DIR}/gen" "${ICONSET_DIR}" "${PREVIEW_PNG}"

mkdir -p "$(dirname "${OUT_ICNS}")"
iconutil -c icns "${ICONSET_DIR}" -o "${OUT_ICNS}"
echo "Generated: ${OUT_ICNS}"
ls -lh "${OUT_ICNS}"
