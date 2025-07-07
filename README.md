This project is built ontop of from: 

https://github.com/surekap/MMM-ShairportMetadata/blob/master/shairport-metadata.py

This Python3 script provides metadata from shairport-sync resizes an image to 32x32 image and sends it to my led matrix over http that displays the artwork. it uses the imageProcessor class to get the top 4 colors in the image artwork and control my homelights over zigbee2mqtt in homeassistant and set the room color the the color theme in the artwork.

<p align="center">
  <img src="assets/matrix.JPG" alt="LED Matrix Artwork" width="256"/><br>
  <i>Real-time album artwork sent to a 64x32 LED matrix</i>
</p>