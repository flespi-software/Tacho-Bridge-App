<template>
  <q-page class="row items-center justify-evenly fit">
    <TachoMainComponent />
  </q-page>
</template>

<script setup lang="ts">
import TachoMainComponent from '../components/TachoMainComponent.vue'

defineOptions({
  name: 'IndexPage',
})
</script>
