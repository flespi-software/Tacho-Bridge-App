<template>
  <!-- Exactly the window height below the header: the card list scrolls inside
       its own area, so the search and filters stay in view. -->
  <q-page class="column" :style-fn="fillViewport">
    <TachoMainComponent />
  </q-page>
</template>

<script setup lang="ts">
import TachoMainComponent from '../components/TachoMainComponent.vue'

defineOptions({
  name: 'IndexPage',
})

// Sized in CSS rather than from the height Quasar passes in: that value trails
// a shrinking window by a resize event, so the page overflowed for a frame and
// a page scrollbar flickered in and out while the window was being resized.
function fillViewport(offset: number) {
  return { height: `calc(100vh - ${offset}px)` }
}
</script>
