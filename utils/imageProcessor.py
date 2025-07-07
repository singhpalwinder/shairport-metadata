import cv2
from sklearn.cluster import KMeans
import numpy as np
from PIL import Image, ImageEnhance

class ImageProcessor:
    def __init__(self, imgPath=None):
        self.imgPath = imgPath
        self.img = None
        self.np_image = None
        if imgPath:
            self.load_image()
    def load_image(self, width=32, height=32):
        self.img = Image.open(self.imgPath).resize((width, height), Image.LANCZOS).convert('RGB')
        self.np_image = np.array(self.img)

    def enhance_image(self, increaseSaturation=True, reduceBrightness=True,increaseContrast=True):
        if self.img is None:
            raise ValueError("Image not loaded. Call load_image() first.")
        img = self.img
        if increaseSaturation:
            img = ImageEnhance.Color(img).enhance(1.5)
        if reduceBrightness:
            img = ImageEnhance.Brightness(img).enhance(0.4)
        if increaseContrast:
            img = ImageEnhance.Contrast(img).enhance(1.5)
        
        # dont need to update the image array since this is only to display the image on matrix.
        # other calculations should be perofrmed from original image
        img = np.array(img)
        return self.floyd_steinberg_dither(img)

    def floyd_steinberg_dither(self, img_np, color_depth_bits=5):
        """Apply Floyd–Steinberg dithering to RGB image with specified per-channel bit depth"""
        height, width, _ = img_np.shape
        out = img_np.astype(np.float32)

        max_val = 2**color_depth_bits - 1

        def quantize(val):
            return round(val * max_val / 255) * (255 / max_val)

        for y in range(height):
            for x in range(width):
                old_pixel = out[y, x].copy()
                new_pixel = np.array([quantize(c) for c in old_pixel])
                out[y, x] = new_pixel
                quant_error = old_pixel - new_pixel

                if x + 1 < width:
                    out[y, x + 1] += quant_error * 7 / 16
                if y + 1 < height:
                    if x > 0:
                        out[y + 1, x - 1] += quant_error * 3 / 16
                    out[y + 1, x] += quant_error * 5 / 16
                    if x + 1 < width:
                        out[y + 1, x + 1] += quant_error * 1 / 16

        return np.clip(out, 0, 255).astype(np.uint8)
    def dominant_color(self, k=4):
        """
        Receives a NumPy image array (RGB format) and k (number of colors to find).
        Returns the most dominant RGB color using KMeans clustering.

        """
        np_img = self.np_image.astype(np.uint8) 
        # Optional: resize inside here if you want to ensure consistent performance
        resized = cv2.resize(np_img, (50, 50), interpolation=cv2.INTER_AREA)

        # Reshape to a list of pixels
        pixel_data = resized.reshape((-1, 3))

        # Apply KMeans clustering
        kmeans = KMeans(n_clusters=k, random_state=0).fit(pixel_data)

        # Get the colors (cluster centers) and their counts
        colors = kmeans.cluster_centers_.astype(int)
        labels = kmeans.labels_
        counts = np.bincount(labels)

        # Sort by frequency
        sorted_idx = np.argsort(counts)[::-1]
        dominant = colors[sorted_idx[0]]
        r,g,b = dominant
        valid_colors = {}
        
        
        print(f"Dominant color (RGB): ({r}, {g}, {b})")
        print(f"Top {k} colors: {colors[sorted_idx]}")
        for colors in colors[sorted_idx]:
            r,g,b=colors
            variance=self.color_variance((r, g, b))
            
            # if color is not black or white and the diff between colors is not alot ie white colors return it 
            if not self.is_blk_white((r, g, b)) and variance > 5:
                #print(f"returning color ({r}, {g}, {b}) with variance: {variance}")
                #chosen_color= (int(r), int(g), int(b))
                valid_colors[int(variance)]=(int(r), int(g), int(b))
        chosen_color, chosen_color_variance=sorted(valid_colors.items())[-1][1], sorted(valid_colors.items())[-1][0]
        print(sorted(valid_colors.items()))
        if chosen_color:
            print(f"returning chosen color {chosen_color} with variance {chosen_color_variance}")
            return chosen_color
        else:
            print(f"Could not determine dominant color defaulting to pinkish/red")
            return (3,137,2)
    def is_blk_white(self, rgb):
        r, g, b = rgb
        brightness = (r + g + b) / 3
        color_spread = max(r, g, b) - min(r, g, b)
        return (brightness < 15) or (brightness > 220 and color_spread < 20)
    def color_variance(self, rgb):
        """Use coefficient of variance to determine variance between rgb colors """
        mean = np.mean(rgb)
        std = np.std(rgb)
        cv = (std / mean) * 100

        return cv
    def get_img_np(self):
        return self.np_image
