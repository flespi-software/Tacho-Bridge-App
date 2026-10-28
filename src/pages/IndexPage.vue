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

function fillViewport(offset: number, height: number) {
  return { height: `${height - offset}px` }
}
</script>
