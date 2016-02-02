vec2 SnapToPixels(vec2 pos)
{
    // Snap the vertex to pixel position to guarantee correct texture
    // sampling when using bilinear filtering.
#ifdef SERVO_ES2
    // TODO(gw): ES2 doesn't have round(). Do we ever get negative coords here?
    return floor(0.5 + pos * uDevicePixelRatio) / uDevicePixelRatio;
#else
    return round(pos * uDevicePixelRatio) / uDevicePixelRatio;
#endif
}

vec2 Bilerp2(vec2 tl, vec2 tr, vec2 br, vec2 bl, vec2 st) {
    return mix(mix(tl, bl, st.y), mix(tr, br, st.y), st.x);
}

vec4 Bilerp4(vec4 tl, vec4 tr, vec4 br, vec4 bl, vec2 st) {
    return mix(mix(tl, bl, st.y), mix(tr, br, st.y), st.x);
}

void main(void)
{
    // Extract the image tiling parameters.
    // These are passed to the fragment shader, since
    // the uv interpolation must be done per-fragment.
    vTileParams = uTileParams[Bottom7Bits(int(aMisc.w))];

    // Determine clip rects.
    vClipOutRect = uClipRects[int(aMisc.z)];
    vec4 clipInRect = uClipRects[int(aMisc.y)];

    // Extract the complete (stacking context + css transform) transform
    // for this vertex. Transform the position by it.
    vec2 offsetParams = uOffsets[Bottom7Bits(int(aMisc.x))].xy;
    mat4 matrix = uMatrixPalette[Bottom7Bits(int(aMisc.x))];

    // Extract the rectangle and snap it to device pixels
    vec2 rect_origin = SnapToPixels(aPositionRect.xy + offsetParams);
    vec2 rect_size = aPositionRect.zw;

    // Determine the position, color, and mask texture coordinates of this vertex.
    vec4 localPos = vec4(0.0, 0.0, 0.0, 1.0);
    bool isBorderCorner = int(aMisc.w) >= 0x80;
    bool isBottomTriangle = IsBottomTriangle();
    if (aPosition.y == 0.0) {
        localPos.y = rect_origin.y;
        if (aPosition.x == 0.0) {
            localPos.x = rect_origin.x;
            if (isBorderCorner) {
                vColor = !isBottomTriangle ? aColorRectTR : aColorRectBL;
            }
        } else {
            localPos.x = rect_origin.x + rect_size.x;
            if (isBorderCorner) {
                vColor = aColorRectTR;
            }
        }
    } else {
        localPos.y = rect_origin.y + rect_size.y;
        if (aPosition.x == 0.0) {
            localPos.x = rect_origin.x;
            if (isBorderCorner) {
                vColor = aColorRectBL;
            }
        } else {
            localPos.x = rect_origin.x + rect_size.x;
            if (isBorderCorner) {
                vColor = !isBottomTriangle ? aColorRectTR : aColorRectBL;
            }
        }
    }

    vec2 localST = (localPos.xy - rect_origin) / rect_size;

    // Rotate or clip as necessary. If there is no rotation, we can clip here in the vertex shader
    // and save a whole bunch of fragment shader invocations. If there is a rotation, we fall back
    // to FS clipping.
    //
    // The rotation angle is encoded as a negative bottom left u coordinate. (uv coordinates should
    // always be nonnegative normally, and gradients don't use color textures, so this is fine.)
    vec4 colorTexCoordRectBottom = aColorTexCoordRectBottom;
    if (colorTexCoordRectBottom.z < 0.0) {
        float angle = -colorTexCoordRectBottom.z;
        vec2 center = rect_origin + rect_size / 2.0;
        vec2 translatedPos = localPos.xy - center;
        localPos.xy = vec2(translatedPos.x * cos(angle) - translatedPos.y * sin(angle),
                           translatedPos.x * sin(angle) + translatedPos.y * cos(angle)) + center;
        colorTexCoordRectBottom.z = aColorTexCoordRectTop.x;
        vClipInRect = clipInRect;
    } else {
        localPos.x = clamp(localPos.x, clipInRect.x, clipInRect.z);
        localPos.y = clamp(localPos.y, clipInRect.y, clipInRect.w);
        vClipInRect = vec4(-1e-37, -1e-37, 1e38, 1e38);
    }

    vColorTexCoord = Bilerp2(aColorTexCoordRectTop.xy, aColorTexCoordRectTop.zw,
                             colorTexCoordRectBottom.xy, colorTexCoordRectBottom.zw,
                             localST);
    vMaskTexCoord = Bilerp2(aMaskTexCoordRectTop.xy, aMaskTexCoordRectTop.zw,
                            aMaskTexCoordRectBottom.xy, aMaskTexCoordRectBottom.zw,
                            localST);
    if (!isBorderCorner) {
        vColor = Bilerp4(aColorRectTL, aColorRectTR, aColorRectBR, aColorRectBL, localST);
    }

    // Normalize the vertex color and mask texture coordinates.
    vColor /= 255.0;
    vMaskTexCoord /= uAtlasParams.zw;

    vPosition = localPos.xy;

    vec4 worldPos = matrix * localPos;

    // Transform by the orthographic projection into clip space.
    gl_Position = uTransform * worldPos;
}

